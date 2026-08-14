# composer-rs Detailed Code Review

> Review date: 2026-08-14
> Baseline: `main` at `8b88596e5107942f1de0feda81a55c644f81c4b6` (`v0.1.1`)
> Worktree state: clean, `main` aligned with `origin/main`
> Scope: the complete repository, including all Rust crates, bundled PHP runtime files, tests, CI, and project documentation
> Review mode: read-only; no source code was changed as part of the review

## 1. Executive summary

The normal Packagist install path is already functional, and the crate boundaries are generally clear. The repository has meaningful coverage for content hashes, authentication, partial updates, mirror failover, archive path traversal, bins, autoload generation, lifecycle scripts, and several CLI commands.

However, the current implementation still has correctness and trust-boundary defects that prevent it from being treated as a safe Composer drop-in replacement. The highest-risk areas are:

- the global CAS is writable through every hardlinked `vendor/` tree, so one project can persistently modify packages used by other projects;
- security auditing fails open when the advisory service is unavailable;
- `--dry-run` executes project lifecycle code before returning;
- root and package conflict semantics are not represented correctly in the solver;
- repository metadata caches are not isolated by repository origin or authentication context;
- stale lock files can be installed successfully while omitting newly declared dependencies;
- `create-project`, classmap-authoritative mode, and custom installer paths can produce internally inconsistent projects or broken autoloaders.

The current state is suitable for controlled experimentation and happy-path use, but shared-cache use across projects, security-gated CI, plugin/installer-heavy projects, and high-concurrency installs require particular caution.

## 2. Architecture observed from code

The real request and installation flow is:

```text
CLI / composer.json
  -> manifest parsing + auth.json / COMPOSER_AUTH
  -> ordered Composer repositories + P2 metadata
  -> PackageIndex + PubGrub dependency solve
  -> composer.lock
  -> path / VCS / dist package installer
  -> archive cache -> extract -> global CAS -> hardlink/copy into vendor
  -> vendor/bin -> autoload/installed metadata -> lifecycle scripts
```

The crate responsibilities are:

- `composer-core`: package identifiers, versions, constraints, platform detection, and shared errors;
- `composer-manifest`: `composer.json`, repositories, installer paths, and Composer content hashes;
- `composer-lock`: `composer.lock` and installed package models;
- `composer-auth`: project/global auth files and `COMPOSER_AUTH`;
- `composer-repo`: Packagist-compatible P2 metadata, search, and disk/memory caches;
- `composer-resolver`: metadata prefetch, virtual packages, partial-update pinning, and PubGrub;
- `composer-download`: dist/path/VCS installation, extraction, bins, mirrors, and checksums;
- `composer-cache`: content-addressable package storage and hardlink-based installation;
- `composer-autoload`: generated Composer runtime files, classmaps, PSR maps, and installed metadata;
- `composer-scripts`: lifecycle scripts, named scripts, shell commands, and PHP callables;
- `composer-cli`: command parsing, orchestration, user-visible output, and exit status.

## 3. Severity definitions

- **High**: can produce an invalid dependency graph, cross-project code contamination, a false security result, successful but incomplete installation, or execution that contradicts an explicit safety flag.
- **Medium**: important compatibility, reliability, portability, or CI weakness with a narrower trigger or a recoverable result.
- **Low**: maintainability or reporting weakness that does not currently dominate runtime correctness.

The reproduction paths below are intended to be turned into regression tests. Unless explicitly stated in the validation section, they were derived from the enforcing source paths and were not all executed during this review.

## 4. Detailed findings

### CR-01 — Global CAS contents are writable through every installed vendor tree

**Severity:** High

**Evidence:**

- `crates/composer-cache/src/lib.rs:93-107` considers an entry valid solely when `.composer-rs-complete` exists.
- `crates/composer-cache/src/lib.rs:154-168` installs an entry using `link_to`.
- `crates/composer-cache/src/lib.rs:374-405` hardlinks every regular CAS file directly into the destination tree.
- There is no read-only permission, copy-on-write break, content rehash, or post-install integrity validation.
- `crates/composer-download/src/bins.rs:66-78` follows a generated bin symlink and changes its target mode, which may be the same CAS inode.

**Trigger and impact:**

Any lifecycle script, application, build tool, or user that modifies a file under `vendor/vendor/package` modifies the shared CAS inode. Every other worktree hardlinked to that inode sees the same modification, and future cache hits continue installing the modified content because the complete marker remains present. This turns a project-local vendor write into persistent cross-project code contamination.

**Minimal reproduction path:**

1. Use one `COMPOSER_RS_CACHE` directory for two projects with the same locked package.
2. Install both projects.
3. Confirm that the same package file has the same inode in the CAS and both vendor trees:

   ```bash
   stat -c '%i %n' \
     "$COMPOSER_RS_CACHE"/cas/*/*/src/Example.php \
     project-a/vendor/vendor/package/src/Example.php \
     project-b/vendor/vendor/package/src/Example.php
   ```

4. Modify `project-a/vendor/vendor/package/src/Example.php`.
5. Observe that the CAS file and project B changed without reinstalling project B.
6. Delete project B's vendor tree and reinstall; the modified content is installed again from the still-complete CAS entry.

A deterministic unit-level reproduction can use `CasCache::store`, call `link_to` twice, write through the first destination, and assert that the second destination and `CasCache::get` now contain the modified bytes.

### CR-02 — CAS and archive publication are not safe across processes

**Severity:** High

**Evidence:**

- `crates/composer-cache/src/lib.rs:49-73` stores locks inside each `CasCache` instance; they do not coordinate separate processes or independently constructed caches.
- `crates/composer-cache/src/lib.rs:121-148` uses one deterministic `.staging` path, deletes existing staging/final paths, renames staging to final, and writes the complete marker only after the rename.
- `crates/composer-cache/src/lib.rs:266-275` treats markerless entries and staging directories as prune candidates.
- `crates/composer-download/src/lib.rs:516-534` derives one archive and one `.partial` path from the URL. Concurrent downloaders create/truncate and rename the same partial file.
- `crates/composer-cache/src/lib.rs:154-168` removes an existing vendor destination and reconstructs it in place rather than publishing a complete replacement atomically.

**Trigger and impact:**

Two `composer-rs` processes installing the same package into different worktrees can delete each other's staging directory, fail during rename, consume a partial archive, or expose a markerless final CAS directory to `cache prune`. A failed `link_to` can leave a partially reconstructed vendor package after the previous destination was already removed.

**Reproduction path:**

1. Prepare two projects with the same lock and an empty shared cache.
2. Start both installs simultaneously in a loop:

   ```bash
   export COMPOSER_RS_CACHE="$(mktemp -d)"
   for attempt in $(seq 1 50); do
     rm -rf "$COMPOSER_RS_CACHE" project-a/vendor project-b/vendor
     (cd project-a && "$REVIEW_BIN" install --no-scripts) &
     pid_a=$!
     (cd project-b && "$REVIEW_BIN" install --no-scripts) &
     pid_b=$!
     wait "$pid_a" "$pid_b" || break
   done
   ```

3. In a third process, repeatedly run `cache prune` to exercise the markerless rename window.
4. Check for rename errors, missing files, partial package trees, or an install that succeeds with different contents between the projects.

### CR-03 — Security audit fails open when packages were not checked

**Severity:** High

**Evidence:**

- `crates/composer-cli/src/commands/audit.rs:83-98` converts transport failure, non-success HTTP status, and invalid JSON into warnings followed by `continue`.
- `crates/composer-cli/src/commands/audit.rs:125-145` treats an empty findings vector as a clean audit and returns success.
- `crates/composer-cli/src/commands/install.rs:250-258` and `update.rs:264-272` use the same audit function for `--audit`.

**Trigger and impact:**

If DNS, TLS, the advisory API, a proxy, or response parsing fails, `composer-rs audit` can exit 0 and print `No known security advisories for locked packages`. CI cannot distinguish a verified clean result from an audit in which no package was checked. The same fail-open behavior applies to `install --audit` and `update --audit`.

**Minimal reproduction path:**

1. Use any project with at least one package in `composer.lock`.
2. Point the audit API to a closed local port:

   ```bash
   COMPOSER_RS_AUDIT_URL=http://127.0.0.1:9/ "$REVIEW_BIN" audit
   echo "$?"
   ```

3. Observe a connectivity warning followed by a clean message and exit status 0.
4. Repeat with a local server returning HTTP 500 and with a server returning invalid JSON; both paths also continue to a successful result.

### CR-04 — `--dry-run` executes lifecycle scripts and skips final platform checks

**Severity:** High

**Evidence:**

- `crates/composer-cli/src/commands/install.rs:78-84` executes `pre-install-cmd`; the dry-run return is at lines 136-145.
- `crates/composer-cli/src/commands/update.rs:127-133` executes `pre-update-cmd`; the dry-run return is at lines 165-174.
- Root and locked-package platform checks occur only after those dry-run returns: `install.rs:147-166` and `update.rs:176-195`.

**Trigger and impact:**

A command advertised as a plan-only operation can execute arbitrary shell or PHP project code, modify files, access credentials, and use the network. It can also present a successful plan that would later fail the skipped root platform check.

**Minimal reproduction path:**

Create a project with an empty lock and the following script:

```json
{
  "name": "review/dry-run",
  "scripts": {
    "pre-install-cmd": "php -r \"file_put_contents('dry-run-ran.txt', 'executed');\""
  }
}
```

Then run:

```bash
"$REVIEW_BIN" install --dry-run
test -f dry-run-ran.txt
```

The marker file is created even though the command reports `Dry run complete (no changes)`. The same test can be repeated with `pre-update-cmd` and `update --dry-run`.

### CR-05 — A stale lock can be installed successfully while omitting current root requirements

**Severity:** High

**Evidence:**

- `crates/composer-cli/src/commands/install.rs:86-96` treats content-hash mismatch as warning-only.
- `crates/composer-cli/src/commands/install.rs:121-145` derives the complete install plan only from the existing lock.
- No subsequent check proves that every current root requirement exists in the lock and satisfies its constraint.

**Trigger and impact:**

After `composer.json` gains or changes a requirement without regenerating the lock, `install` can exit 0 while leaving the declared dependency absent. The generated installed marker and success message make the incomplete installation look valid.

**Deterministic reproduction path:**

Use a root manifest that requires an inline package:

```json
{
  "name": "review/stale-lock",
  "repositories": [
    {
      "type": "package",
      "package": {"name": "acme/required", "version": "1.0.0", "type": "metapackage"}
    }
  ],
  "require": {"acme/required": "1.0.0"}
}
```

Create a stale lock containing no packages:

```json
{
  "content-hash": "deadbeef",
  "packages": [],
  "packages-dev": []
}
```

Run:

```bash
"$REVIEW_BIN" install --no-scripts --no-autoloader
echo "$?"
```

The command warns about the hash but can return 0 after installing zero packages, even though `acme/required` is a current root requirement.

### CR-06 — Root `conflict` declarations are absent from the solve request

**Severity:** High

**Evidence:**

- `crates/composer-manifest/src/lib.rs:92-93` parses root `conflict`.
- `crates/composer-resolver/src/lib.rs:207-217` passes root dependencies, replace, and provide into `SolveRequest`, but not conflict.
- `crates/composer-resolver/src/provider.rs:20-31` has no field for root conflicts.
- `crates/composer-resolver/src/provider.rs:161-180` validates only conflicts declared by selected packages.

**Trigger and impact:**

A package version prohibited directly by the root project can be selected and written to the lock. The resulting graph is accepted by composer-rs but rejected by the root's own dependency policy.

**Deterministic reproduction path:**

```json
{
  "name": "review/root-conflict",
  "repositories": [
    {
      "type": "package",
      "package": {"name": "acme/bad", "version": "1.0.0", "type": "metapackage"}
    }
  ],
  "require": {"acme/bad": "*"},
  "conflict": {"acme/bad": "<2.0"}
}
```

Run `update --no-scripts --no-autoloader`. The solver can select `acme/bad 1.0.0` even though the root explicitly conflicts with it.

### CR-07 — Package conflicts are encoded as positive dependencies

**Severity:** High

**Evidence:**

- `crates/composer-resolver/src/provider.rs:137-158` computes reachability from the dependency union of every available version.
- `crates/composer-resolver/src/provider.rs:267-276` turns a package conflict into a dependency on the complement range.

**Trigger and impact:**

If a package name is reachable through any candidate version, another package's conflict with that name forces the conflicting package name into the graph even when the finally selected version does not require it. This installs unnecessary packages or reports no solution for a valid graph.

**Deterministic reproduction path:**

Use inline packages with this graph:

- `acme/a 1.0.0` requires `acme/b ^1`;
- `acme/a 2.0.0` has no dependencies;
- only `acme/b 1.0.0` exists;
- `acme/c 1.0.0` conflicts with `acme/b ^1`;
- the root requires `acme/a *` and `acme/c *`.

The valid solution is `acme/a 2.0.0 + acme/c 1.0.0` with no B. Current reachability still includes B because of A 1.0.0, and C's conflict becomes a positive requirement for a non-`^1` B version. Since no such B exists, resolution fails.

### CR-08 — Repository metadata cache identity omits repository origin and auth context

**Severity:** High

**Evidence:**

- `crates/composer-repo/src/lib.rs:287-294` derives the disk path from package name only.
- `crates/composer-repo/src/lib.rs:161-170` consults disk cache before constructing or sending the repository request.
- `crates/composer-repo/src/lib.rs:313-319` writes raw repository responses into that shared package-name path.
- Each configured repository client uses the same global metadata directory.

**Trigger and impact:**

Two repositories serving the same package name share one cache entry even when they have different URLs, credentials, versions, dist locations, or repository policies. A prior public response can defeat custom-repository priority, and private metadata can be exposed to another project using the same OS account and cache.

**Reproduction path:**

1. Start two local P2 servers on different ports.
2. Server A returns `acme/shared 1.0.0`; server B returns `acme/shared 2.0.0` with a visibly different dist URL.
3. Use a fresh common `COMPOSER_RS_CACHE` and query server A.
4. Within the ten-minute TTL, create a new `RepositoryClient` for server B and query the same package.
5. Observe that server B is not contacted and the client returns server A's cached metadata.
6. Repeat with A requiring Basic auth and B unauthenticated to demonstrate that the cache is also not scoped by authentication context.

### CR-09 — `packagist.org: false` is undone when no Composer HTTP repository exists

**Severity:** High

**Evidence:**

- `crates/composer-repo/src/lib.rs:366-371` correctly skips the normal Packagist insertion when `manifest.packagist_enabled()` is false.
- `crates/composer-repo/src/lib.rs:373-377` then unconditionally inserts default Packagist whenever the client list is empty.
- Path, VCS, and inline package repositories do not add an HTTP repository client, so a project using only those repository types still reaches this fallback.

**Trigger and impact:**

A project that explicitly disables Packagist can still query public Packagist for a missing package. This violates an explicit repository trust policy and creates a dependency-confusion path for private package names.

**Minimal reproduction path:**

Use:

```json
{
  "name": "review/no-packagist",
  "repositories": {"packagist.org": false},
  "require": {"publicly-visible-or-missing/name": "*"}
}
```

Run `update` while observing DNS/HTTP traffic or pointing `repo.packagist.org` at a local request logger. A request is still made even though Packagist was explicitly disabled.

### CR-10 — Authenticated repository URLs can use plaintext HTTP

**Severity:** High

**Evidence:**

- `crates/composer-repo/src/lib.rs:117-136` accepts any base URL without enforcing HTTPS or reading a `secure-http` policy.
- `crates/composer-repo/src/lib.rs:173-180` applies configured auth to the constructed URL.
- `crates/composer-auth/src/lib.rs:221-234` attaches Basic, Bearer, or token credentials based only on the URL host.

**Trigger and impact:**

If a custom repository is configured as `http://repo.example` and auth exists for that host, credentials and private metadata are transmitted without TLS. The CLI does not reject the URL or require an explicit insecure opt-in.

**Reproduction path:**

1. Run a local plaintext HTTP server that records request headers.
2. Configure a Composer repository using its `http://` URL and add `http-basic` credentials for the host.
3. Run `show`, `require`, or `update` for a package from that repository.
4. Observe the Authorization header in the plaintext server request.

### CR-11 — `create-project` mixes a synthetic root graph with the unpacked project's manifest

**Severity:** High

**Evidence:**

- `crates/composer-cli/src/commands/create_project.rs:53-68` creates a synthetic root that requires the requested project package.
- `create_project.rs:91-106` resolves, saves, and installs that synthetic graph.
- `create_project.rs:118-138` copies a `type:project` package into the target root and reloads its composer.json.
- `create_project.rs:140-150` still generates autoload files from the in-memory synthetic lock.
- Errors from bin installation and `post-install-cmd` are discarded at lines 110-116 and 152-154.

**Trigger and impact:**

For a normal `type:project` package, the final root composer.json is not the manifest used to create the installed graph. If the archive has no lock, the saved synthetic lock has the wrong content hash and contains the project package as its own dependency. If the archive contains a lock, it may overwrite the file on disk, but vendor and autoload still use the synthetic in-memory lock. Missing bins or a failing initialization script do not prevent a success message.

**Reproduction path:**

1. Run `create-project` for any small `type:project` fixture or public package with `--no-scripts`.
2. Inspect the final composer.json and composer.lock.
3. Recalculate the content hash or run `validate` and inspect whether the project package remains in its own lock.
4. Inspect `vendor/composer/installed.json` and autoload maps for entries from the synthetic root graph.
5. Add a deliberately failing `post-install-cmd` to a local project fixture and observe that creation can still report success.

### CR-12 — Classmap-authoritative mode without optimization breaks PSR-only classes

**Severity:** High

**Evidence:**

- `crates/composer-autoload/src/lib.rs:92-105` enters the classmap branch for either optimization or authoritative mode, but scans PSR-4 directories only when `options.optimize` is true.
- `composer-autoload/src/lib.rs:736-787` independently enables `setClassMapAuthoritative(true)`.
- `crates/composer-autoload/php/ClassLoader.php:442-450` returns false on a classmap miss before attempting PSR lookup when authoritative mode is active.
- `install`, `update`, and `dump-autoload` expose optimization and authoritative mode as independent flags.

**Trigger and impact:**

Running with `-a` but without `-o` creates an authoritative loader whose classmap excludes normal PSR-4 classes. Those classes cannot be loaded even though the PSR maps exist.

**Deterministic reproduction path:**

1. Create a path package with `autoload.psr-4` mapping `Acme\\` to `src/` and a class `Acme\\Example` in `src/Example.php`.
2. Require it from a root project.
3. Run:

   ```bash
   "$REVIEW_BIN" update --no-scripts --classmap-authoritative
   php -r "require 'vendor/autoload.php'; var_dump(class_exists('Acme\\\\Example'));"
   ```

4. The class lookup fails. Repeating with both `-a -o` demonstrates that the missing PSR classmap scan is tied to optimization rather than authoritative mode itself.

### CR-13 — Custom installer paths are not used by autoload or installed metadata generation

**Severity:** High

**Evidence:**

- `crates/composer-manifest/src/installer_paths.rs:31-80` defines custom and built-in installer paths.
- `crates/composer-download/src/lib.rs:242-254` uses those paths when selecting the installation destination.
- `crates/composer-autoload/src/lib.rs:267-276` hardcodes the package name as its vendor-relative autoload root.
- `composer-autoload/src/lib.rs:827-852` hardcodes installed.json `install-path` as `../<package-name>`.
- `composer-autoload/src/lib.rs:855-905` hardcodes installed.php paths in the same way.

**Trigger and impact:**

WordPress, Drupal, Craft, TYPO3, or any package selected by `extra.installer-paths` is installed outside `vendor/<name>`, but generated autoload and runtime metadata still point inside vendor. PSR/classmap entries and `Composer\\InstalledVersions::getInstallPath()` can therefore reference nonexistent paths.

**Reproduction path:**

1. Create a path package of type `wordpress-plugin` with a PSR-4 class.
2. Require it from a root project without overriding the built-in installer paths.
3. Run update/install.
4. Confirm the package exists at `wp-content/plugins/<name>` rather than under vendor.
5. Inspect `vendor/composer/autoload_psr4.php`, `installed.json`, and `installed.php`; they reference the vendor path.
6. Attempt to autoload the package class or query its install path through `Composer\\InstalledVersions`.

### CR-14 — Supported constraint tokens `==` and `<>` fail open in PubGrub ranges

**Severity:** High

**Evidence:**

- `crates/composer-core/src/version.rs:281-307` recognizes `==` and `<>` as constraint operators.
- `crates/composer-core/src/ranges.rs:44-98` does not implement either operator.
- The fallback at lines 95-98 returns a full range when parsing fails.

**Trigger and impact:**

A dependency declared as `== 1.2.3` can resolve to a different, usually highest, version. `<> 1.2.3` can also be treated as unrestricted. The separate `VersionConstraint::matches` path does not fail in the same way, so different commands can disagree about the same constraint.

**Deterministic reproduction path:**

1. Define inline versions `acme/pkg 1.0.0` and `acme/pkg 2.0.0`.
2. Require `"acme/pkg": "== 1.0.0"`.
3. Run `update --no-scripts --no-autoloader`.
4. Inspect the lock; the PubGrub range is unrestricted and may select 2.0.0.
5. Repeat with `<> 2.0.0`; the excluded version may still be selected.

### CR-15 — `provide`/`replace: self.version` is converted to virtual version 0.0.0

**Severity:** High

**Evidence:**

- `crates/composer-resolver/src/index.rs:149-166` recognizes several constraint-shaped values but not `self.version`.
- It therefore stores the literal text `self.version` as the virtual version.
- `index.rs:97-100` silently replaces unparseable versions with `0.0.0`.

**Trigger and impact:**

Packages commonly use `self.version` for replaced or provided packages. A provider at 2.3.0 becomes a virtual 0.0.0 entry, so a requirement such as `^2.0` is not satisfied even though the declaration is intended to expose the provider's own 2.3.0 version.

**Deterministic reproduction path:**

Define an inline package:

```json
{
  "name": "acme/provider",
  "version": "2.3.0",
  "type": "metapackage",
  "replace": {"acme/virtual": "self.version"}
}
```

Require both `acme/provider: 2.3.0` and `acme/virtual: ^2.0`. Resolution fails because the registered virtual version is represented as 0.0.0.

### CR-16 — Platform simulation does not implement disabled extensions or extension versions

**Severity:** Medium

**Evidence:**

- `crates/composer-core/src/platform.rs:72-76` handles `config.platform.<name> = false` by continuing without removing an already detected extension.
- `platform.rs:127-135` ignores the extension constraint string and returns true for any loaded extension.
- `platform.rs:116-119` accepts all `lib-*` and `composer-*` packages whenever platform detection is reliable.

**Trigger and impact:**

`"config": {"platform": {"ext-json": false}}` should simulate an unavailable extension, but an actually loaded JSON extension remains in the set and satisfies requirements. Likewise, `ext-example >=999` is accepted whenever the extension is loaded, regardless of its installed version.

**Reproduction path:**

1. On a PHP build with `ext-json`, set `config.platform.ext-json` to false and require `ext-json: *`.
2. Run update/install without an ignore-platform flag.
3. Observe that the requirement is accepted.
4. Replace the constraint with `>=999`; the loaded extension is still accepted because its version is never checked.

### CR-17 — Extra `run-script` arguments are concatenated as shell syntax

**Severity:** Medium

**Evidence:**

- `crates/composer-cli/src/commands/run_script.rs:18-20` accepts arbitrary trailing arguments.
- `crates/composer-scripts/src/lib.rs:295-306` appends each argument verbatim to the configured command.
- `composer-scripts/src/lib.rs:318-326` executes the combined string through `sh -c` or `cmd /C`.
- PHP callable arguments use `Command::arg` and do not have this problem.

**Trigger and impact:**

An argument intended to be data becomes shell syntax. This is unsafe when a CI job, web wrapper, or higher-level tool forwards user-controlled arguments to `composer-rs run-script`.

**Minimal reproduction path:**

```json
{
  "name": "review/script-args",
  "scripts": {"print-arg": "printf '%s\\n'"}
}
```

Run:

```bash
"$REVIEW_BIN" run-script print-arg -- 'value; touch injected-by-arg'
test -f injected-by-arg
```

The `touch` command is parsed by the shell instead of being passed as literal data.

### CR-18 — `require` and `remove` leave the manifest changed when update fails

**Severity:** Medium

**Evidence:**

- `crates/composer-cli/src/commands/require.rs:93-118` saves composer.json before the fallible update.
- `crates/composer-cli/src/commands/remove.rs:50-75` uses the same ordering.
- There is no manifest backup, transaction, or rollback path.

**Trigger and impact:**

Solver conflicts, repository outages, checksum failures, platform failures, download errors, autoload failures, or lifecycle script failures leave composer.json modified while composer.lock and vendor may still represent the previous graph or a partially applied new graph. A failed command therefore changes the starting state of the next retry.

**Reproduction path:**

1. Run `require acme/package` where the package or one of its dependencies is guaranteed to fail resolution or download.
2. Observe a nonzero command exit.
3. Inspect composer.json and verify that the failed requirement remains present.
4. For `remove`, arrange a root graph that becomes unsatisfiable after removing one requirement and observe that the removed key is not restored.

### CR-19 — Archive extraction has no resource limits and does not preserve link semantics

**Severity:** Medium

**Evidence:**

- `crates/composer-download/src/extract.rs:62-93` copies every zip entry without entry-count, expanded-size, path-depth, or compression-ratio limits.
- `extract.rs:96-130` does the same for tar archives.
- Tar symlink/hardlink entries and zip Unix symlinks are opened as ordinary files rather than reconstructed as links.
- `crates/composer-download/src/lib.rs:797-829` also omits symlinks when copying path/VCS trees.

**Trigger and impact:**

A malicious or corrupted dist archive can exhaust disk space or inodes before CAS publication. Legitimate packages containing symlinks or hardlinks are installed with different filesystem semantics and may fail at runtime.

**Reproduction path:**

1. Build a small compressed archive whose entries expand to a very large sparse/repeated payload or contain hundreds of thousands of files.
2. Serve it as an inline package dist and run install with an isolated cache and filesystem quota.
3. Observe that extraction continues until an OS-level resource error.
4. Create a tar package containing a symlink and install it; inspect the destination with `stat`/`readlink` and observe that the link was converted to a regular file.

### CR-20 — VCS discovery and installation trust incomplete cache directories

**Severity:** Medium

**Evidence:**

- `crates/composer-resolver/src/sources.rs:159-175` treats an existing `.git` directory as a valid checkout and ignores the result of `git fetch`.
- The resolver does not reset or check out a fetched tag/branch and indexes only the current working tree.
- `crates/composer-download/src/lib.rs:701-767` similarly performs clone/fetch/checkout only when `.git` is absent.
- There is no completion marker, process lock, or `rev-parse` verification against the locked reference before copying the tree.

**Trigger and impact:**

An interrupted clone or checkout can leave `.git` behind, causing the next install to copy a default-branch, stale, or partial working tree. Resolver refreshes may fetch new refs but continue reading the old checkout. Tagged and non-default-branch versions are not represented with normal Composer VCS semantics.

**Reproduction path:**

1. Start a VCS install and interrupt it after `.git` has been created but before checkout completes.
2. Run the same install again and inspect whether the clone/checkout steps are skipped.
3. For resolver freshness, populate the VCS cache, advance the remote default branch, rerun update, and compare the cached working tree HEAD with the fetched remote HEAD.
4. Request a tag or non-default branch that is not the current checkout and observe that discovery exposes only the cached working tree's manifest/version.

### CR-21 — Lockfile round-trips drop unknown Composer fields

**Severity:** Medium

**Evidence:**

- `crates/composer-lock/src/lib.rs:12-50` models only known top-level lock fields and has no flattened storage for unknown fields.
- `composer-lock/src/lib.rs:201-278` does the same for package entries.
- `ComposerJson` explicitly preserves unknown fields, but the lock model does not.

**Trigger and impact:**

Loading and saving a lock produced by official Composer can delete valid package or transport metadata not represented by `LockedPackage`. Commands such as update, require, remove, or `update --lock` can therefore create a noisy and potentially semantically lossy lock rewrite unrelated to the requested change.

**Reproduction path:**

1. Add a synthetic unknown top-level key and an unknown package key to a valid composer.lock.
2. Load and save it through `ComposerLock`, or run a command that rewrites the lock.
3. Diff the result and observe that both unknown fields disappear.

### CR-22 — Diagnostic and freshness commands can report success after incomplete checks

**Severity:** Medium

**Evidence:**

- `crates/composer-cli/src/commands/diagnose.rs:13-66` records missing PHP, unwritable cache, and Packagist failures in `ok`.
- `diagnose.rs:91-96` only prints a warning and always returns `Ok(())`.
- `crates/composer-cli/src/commands/outdated.rs:226-256` warns and skips packages whose metadata could not be fetched.
- `outdated.rs:258-285` can print `All packages are up to date`, and `--strict` counts only successfully checked rows.

**Trigger and impact:**

Automation cannot use `diagnose` as a health gate. `outdated --strict` can exit 0 when one or all packages were never checked, making repository outages indistinguishable from a fully up-to-date lock.

**Reproduction path:**

1. Block Packagist access and run `diagnose`; observe warnings with exit status 0.
2. Use a lock with one package and a repository URL that returns HTTP 500.
3. Run `outdated --strict`; observe that the package is skipped and the command can report everything up to date with status 0.

### CR-23 — CI does not validate pull requests or normal branch pushes

**Severity:** Medium

**Evidence:**

- `.github/workflows/ci.yml:3-6` triggers only on `v*` tag pushes and manual dispatch.
- Tests, Clippy, format checks, release builds, and publication are all part of the same tag workflow.
- `.github/workflows/ci.yml:23-29`, 59-64, 77, and 91-94 use mutable action/toolchain tags.
- The workspace declares Rust 1.86 in `Cargo.toml:20`, but CI installs only stable.
- The build matrix has Linux and macOS only, despite platform-specific Windows code paths.

**Trigger and impact:**

Ordinary changes can merge without any automated validation. The first automatic signal may occur after a release tag has already been created. The advertised MSRV and Windows branches are not verified, and a mutable third-party action tag can affect a release job with `contents: write`.

**Reproduction path:**

1. Open a pull request or push a normal branch commit and inspect GitHub Actions; the workflow does not start.
2. Attempt a local build with Rust 1.86 and compare it with the stable-only CI result.
3. Run the CLI/script tests on Windows to exercise the `cmd /C`, PATH separator, bin proxy, and filesystem branches that are absent from the current matrix.

## 5. Coverage strengths observed

The review also found useful existing invariants:

- Composer content-hash behavior is checked against PHP-derived fixtures, encoded bytes, key ordering, and `config.platform` participation.
- Authentication tests cover environment/file precedence, HTTP Basic, GitLab private/deploy/CI tokens, OAuth, and header selection.
- Resolver tests cover a basic diamond, partial update pinning, `-w`/`-W`, project auth, direct package conflicts, and one virtual provider path.
- Download integration tests cover normal path/dist installs, mirror failover, GitLab auth, bin links, autoload, and a lifecycle script.
- Archive paths reject lexical `..` traversal.
- Most library crates use `#![deny(unsafe_code)]`.

The main testing gap is not the absence of happy-path tests; it is the lack of adversarial, cross-process, failure-path, and compatibility-boundary tests for the findings above.

## 6. Validation performed for this review

The following commands were run against the stated baseline:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo audit
```

Results:

- formatting check passed;
- Clippy exited successfully but emitted existing warnings because CI does not use `-D warnings`;
- all workspace tests passed: 131 passed, 0 failed;
- `cargo audit` found no known vulnerabilities in 275 locked dependencies;
- `cargo audit` reported one allowed unmaintained warning: `number_prefix 0.4.0`, introduced by `indicatif 0.17.11`;
- validation environment: Rust 1.97.1, Cargo 1.97.1, PHP 8.5.8, Linux aarch64;
- the Rust 1.86 MSRV, Windows behavior, and differential testing against official Composer were not run;
- the worktree remained clean after validation.

## 7. Review verdict

The project has a coherent architecture and a meaningful functional baseline, but the current shared-cache trust model and several solver/CLI semantics are not yet safe enough for broad Composer-compatible production use.

The most urgent correctness and safety boundaries are CR-01 through CR-09. CR-11 through CR-15 affect common advertised workflows and should also be treated as release-blocking for any claim of broad Composer parity. The remaining findings primarily concern compatibility, failure isolation, portability, and release assurance.
