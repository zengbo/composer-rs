//! Composer scripts runner (`scripts` in composer.json).
//!
//! Supports shell lines, `@script` references, `@php …`, and PHP callables
//! (`ClassName::method`) matching official Composer's EventDispatcher.

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_manifest::ComposerJson;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info};

/// Lifecycle events composer-rs can fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptEvent {
    PreInstallCmd,
    PostInstallCmd,
    PreUpdateCmd,
    PostUpdateCmd,
    PreAutoloadDump,
    PostAutoloadDump,
}

impl ScriptEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreInstallCmd => "pre-install-cmd",
            Self::PostInstallCmd => "post-install-cmd",
            Self::PreUpdateCmd => "pre-update-cmd",
            Self::PostUpdateCmd => "post-update-cmd",
            Self::PreAutoloadDump => "pre-autoload-dump",
            Self::PostAutoloadDump => "post-autoload-dump",
        }
    }
}

/// Run a named script from the manifest (or a lifecycle event name).
pub fn run_script(
    manifest: &ComposerJson,
    name: &str,
    project_root: &Path,
    extra_args: &[String],
    dev_mode: bool,
) -> Result<()> {
    let scripts = match &manifest.scripts {
        Some(s) => s,
        None => {
            return Err(Error::other(format!("script `{name}` is not defined")));
        }
    };
    if !scripts.contains_key(name) {
        return Err(Error::other(format!("script `{name}` is not defined")));
    }
    let mut stack = BTreeSet::new();
    run_named(
        scripts,
        name,
        project_root,
        &script_dirs(manifest, project_root),
        extra_args,
        dev_mode,
        &mut stack,
    )
}

/// Fire a lifecycle event if defined (no error when missing).
pub fn run_event(
    manifest: &ComposerJson,
    event: ScriptEvent,
    project_root: &Path,
    dev_mode: bool,
) -> Result<()> {
    let Some(scripts) = &manifest.scripts else {
        return Ok(());
    };
    if !scripts.contains_key(event.as_str()) {
        return Ok(());
    }
    info!(event = event.as_str(), "running composer script");
    let mut stack = BTreeSet::new();
    run_named(
        scripts,
        event.as_str(),
        project_root,
        &script_dirs(manifest, project_root),
        &[],
        dev_mode,
        &mut stack,
    )
}

/// List script names defined in the manifest.
pub fn list_scripts(manifest: &ComposerJson) -> Vec<String> {
    manifest
        .scripts
        .as_ref()
        .map(|s| s.keys().cloned().collect())
        .unwrap_or_default()
}

struct ScriptDirs {
    vendor: PathBuf,
    bin: PathBuf,
}

fn script_dirs(manifest: &ComposerJson, project_root: &Path) -> ScriptDirs {
    ScriptDirs {
        vendor: project_root.join(manifest.vendor_dir()),
        bin: project_root.join(manifest.bin_dir()),
    }
}

fn run_named(
    scripts: &BTreeMap<String, Value>,
    name: &str,
    project_root: &Path,
    dirs: &ScriptDirs,
    extra_args: &[String],
    dev_mode: bool,
    stack: &mut BTreeSet<String>,
) -> Result<()> {
    if !stack.insert(name.to_string()) {
        return Err(Error::other(format!(
            "script cycle detected involving `{name}`"
        )));
    }
    let Some(def) = scripts.get(name) else {
        stack.remove(name);
        return Err(Error::other(format!("script `{name}` is not defined")));
    };
    let commands = expand_commands(def);
    for (i, cmd) in commands.iter().enumerate() {
        let is_last = i + 1 == commands.len();
        let args = if is_last { extra_args } else { &[] };
        if let Some(ref_name) = cmd.strip_prefix('@') {
            // @php ... is not a script reference
            if ref_name.starts_with("php ") || ref_name == "php" {
                run_shell_line(cmd, project_root, dirs, args, dev_mode)?;
            } else {
                // @script-name or @script-name arg1
                let mut parts = ref_name.split_whitespace();
                let ref_script = parts.next().unwrap_or("");
                let mut ref_args: Vec<String> = parts.map(|s| s.to_string()).collect();
                ref_args.extend(args.iter().cloned());
                run_named(
                    scripts,
                    ref_script,
                    project_root,
                    dirs,
                    &ref_args,
                    dev_mode,
                    stack,
                )?;
            }
        } else if let Some((class, method)) = split_php_callable(cmd) {
            run_php_callable(
                PhpCallable {
                    original: cmd,
                    class,
                    method,
                },
                name,
                project_root,
                dirs,
                args,
                dev_mode,
            )?;
        } else {
            run_shell_line(cmd, project_root, dirs, args, dev_mode)?;
        }
    }
    stack.remove(name);
    Ok(())
}

fn expand_commands(def: &Value) -> Vec<String> {
    match def {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Composer: `isPhpScript` — no spaces, contains `::`.
fn split_php_callable(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.contains(' ') || !line.contains("::") {
        return None;
    }
    line.split_once("::")
}

fn php_binary() -> String {
    std::env::var("PHP_BINARY")
        .or_else(|_| std::env::var("COMPOSER_PHP"))
        .unwrap_or_else(|_| "php".into())
}

fn materialize_callable_runner() -> Result<PathBuf> {
    const SRC: &str = include_str!("../php/callable_runner.php");
    let path = std::env::temp_dir().join("composer-rs-callable-runner-v1.php");
    let need_write = std::fs::read_to_string(&path)
        .map(|existing| existing != SRC)
        .unwrap_or(true);
    if need_write {
        std::fs::write(&path, SRC)
            .map_err(|e| Error::other(format!("failed to write PHP callable runner: {e}")))?;
    }
    Ok(path)
}

struct PhpCallable<'a> {
    original: &'a str,
    class: &'a str,
    method: &'a str,
}

fn run_php_callable(
    spec: PhpCallable<'_>,
    event_name: &str,
    project_root: &Path,
    dirs: &ScriptDirs,
    extra_args: &[String],
    dev_mode: bool,
) -> Result<()> {
    // Composer\Config::disableProcessTimeout only affects the in-process
    // ProcessExecutor timeout. We do not impose one, so this is a no-op.
    if spec.class == "Composer\\Config" && spec.method == "disableProcessTimeout" {
        debug!("Composer\\Config::disableProcessTimeout (no-op)");
        return Ok(());
    }

    let runner = materialize_callable_runner()?;
    let php = php_binary();
    let vendor = dirs.vendor.to_string_lossy();
    let bin = dirs.bin.to_string_lossy();

    debug!(callable = %spec.original, "exec PHP callable");
    eprintln!("> {}::{}", spec.class, spec.method);

    let mut cmd = Command::new(&php);
    cmd.arg(&runner)
        .arg(vendor.as_ref())
        .arg(bin.as_ref())
        .arg(spec.class)
        .arg(spec.method)
        .arg(event_name)
        .arg(if dev_mode { "1" } else { "0" });
    for a in extra_args {
        cmd.arg(a);
    }
    apply_script_env(&mut cmd, project_root, dirs, dev_mode);
    cmd.env("PHP_BINARY", &php);

    let status = cmd.status().map_err(|e| {
        Error::other(format!(
            "failed to spawn php for script `{}`: {e}",
            spec.original
        ))
    })?;
    let code = status.code().unwrap_or(1);
    // 2 = class not autoloadable, 3 = method not callable: Composer warns and continues.
    if code == 2 {
        eprintln!(
            "warning: Class {} is not autoloadable, can not call {event_name} script",
            spec.class
        );
        return Ok(());
    }
    if code == 3 {
        eprintln!(
            "warning: Method {}::{} is not callable, can not call {event_name} script",
            spec.class, spec.method
        );
        return Ok(());
    }
    if !status.success() {
        return Err(Error::other(format!(
            "script `{}` failed with {status}",
            spec.original
        )));
    }
    Ok(())
}

fn run_shell_line(
    line: &str,
    project_root: &Path,
    dirs: &ScriptDirs,
    extra_args: &[String],
    dev_mode: bool,
) -> Result<()> {
    let mut full = line.to_string();
    for a in extra_args {
        full.push(' ');
        full.push_str(&shell_quote(a));
    }
    debug!(cmd = %full, "exec script");

    // @php foo.php → php foo.php
    let cmd_line = if let Some(rest) = full.strip_prefix("@php ") {
        format!("php {rest}")
    } else if full == "@php" {
        "php".into()
    } else {
        full
    };

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&cmd_line);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&cmd_line);
        c
    };
    apply_script_env(&mut cmd, project_root, dirs, dev_mode);

    let status = cmd
        .status()
        .map_err(|e| Error::other(format!("failed to spawn script `{cmd_line}`: {e}")))?;
    if !status.success() {
        return Err(Error::other(format!(
            "script `{cmd_line}` failed with {status}"
        )));
    }
    Ok(())
}

fn shell_quote(arg: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn apply_script_env(cmd: &mut Command, project_root: &Path, dirs: &ScriptDirs, dev_mode: bool) {
    cmd.current_dir(project_root);
    cmd.env("COMPOSER_DEV_MODE", if dev_mode { "1" } else { "0" });
    if let Ok(bin) = std::env::current_exe() {
        cmd.env("COMPOSER", bin);
    }
    cmd.env("COMPOSER_BINARY", "composer-rs");
    prepend_bin_dir(cmd, &dirs.bin);
}

fn prepend_bin_dir(cmd: &mut Command, bin_dir: &Path) {
    if !bin_dir.is_dir() {
        return;
    }
    let bin_dir = match bin_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut entries = vec![bin_dir];
    if let Some(existing) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(entries) {
        cmd.env("PATH", joined);
    }
}

/// Resolve script references into an ordered list of shell commands (for dry-run).
pub fn expand_script_commands(manifest: &ComposerJson, name: &str) -> Result<Vec<String>> {
    let scripts = manifest
        .scripts
        .as_ref()
        .ok_or_else(|| Error::other(format!("script `{name}` is not defined")))?;
    let mut out = Vec::new();
    let mut queue = VecDeque::from([name.to_string()]);
    let mut seen = BTreeSet::new();
    while let Some(n) = queue.pop_front() {
        if !seen.insert(n.clone()) {
            return Err(Error::other(format!("script cycle involving `{n}`")));
        }
        let def = scripts
            .get(&n)
            .ok_or_else(|| Error::other(format!("script `{n}` is not defined")))?;
        for cmd in expand_commands(def) {
            if let Some(r) = cmd.strip_prefix('@') {
                if r.starts_with("php ") || r == "php" {
                    out.push(cmd);
                } else {
                    let ref_script = r.split_whitespace().next().unwrap_or("");
                    queue.push_back(ref_script.to_string());
                }
            } else {
                out.push(cmd);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_with(scripts: Value) -> ComposerJson {
        let v = json!({
            "name": "acme/app",
            "scripts": scripts
        });
        serde_json::from_value(v).unwrap()
    }

    fn php_available() -> bool {
        Command::new(php_binary())
            .arg("-v")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn extra_args_are_shell_quoted() {
        assert_eq!(
            shell_quote("value; touch injected"),
            "'value; touch injected'"
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn detects_php_callables() {
        assert_eq!(
            split_php_callable(r"Illuminate\Foundation\ComposerScripts::postAutoloadDump"),
            Some((r"Illuminate\Foundation\ComposerScripts", "postAutoloadDump"))
        );
        assert_eq!(
            split_php_callable(r"Composer\Config::disableProcessTimeout"),
            Some((r"Composer\Config", "disableProcessTimeout"))
        );
        assert!(split_php_callable("echo foo::bar").is_none());
        assert!(split_php_callable("@php artisan").is_none());
        assert!(split_php_callable("phpunit").is_none());
    }

    #[test]
    fn expands_references() {
        let m = manifest_with(json!({
            "test": ["@phpunit"],
            "phpunit": "echo phpunit"
        }));
        let cmds = expand_script_commands(&m, "test").unwrap();
        assert_eq!(cmds, vec!["echo phpunit"]);
    }

    #[test]
    fn expands_php_callables_as_themselves() {
        let m = manifest_with(json!({
            "post-autoload-dump": [
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                "@php artisan package:discover"
            ]
        }));
        let cmds = expand_script_commands(&m, "post-autoload-dump").unwrap();
        assert_eq!(
            cmds,
            vec![
                r"Illuminate\Foundation\ComposerScripts::postAutoloadDump",
                "@php artisan package:discover"
            ]
        );
    }

    #[test]
    fn runs_echo_script() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manifest_with(json!({
            "post-autoload-dump": "echo ok-from-script > ran.txt"
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, tmp.path(), true).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("ran.txt")).unwrap();
        assert!(body.contains("ok-from-script"));
    }

    #[test]
    fn disable_process_timeout_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manifest_with(json!({
            "post-autoload-dump": [
                "Composer\\Config::disableProcessTimeout",
                "echo after-timeout > ran.txt"
            ]
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, tmp.path(), true).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("ran.txt")).unwrap();
        assert!(body.contains("after-timeout"));
    }

    #[test]
    fn missing_php_callable_warns_and_continues() {
        if !php_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let m = manifest_with(json!({
            "post-autoload-dump": [
                "Does\\Not\\Exist::missing",
                "echo continued > ran.txt"
            ]
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, tmp.path(), true).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("ran.txt")).unwrap();
        assert!(body.contains("continued"));
    }

    #[test]
    fn callable_runner_loads_files_autoload_before_handler() {
        if !php_available() {
            return;
        }
        use composer_autoload::{AutoloadOptions, generate};
        use composer_core::AutoloadConfig;
        use composer_lock::{ComposerLock, DistInfo, LockedPackage};
        use std::collections::BTreeMap;

        fn locked_pkg(name: &str, autoload: AutoloadConfig) -> LockedPackage {
            LockedPackage {
                name: name.into(),
                version: "1.0.0".into(),
                source: None,
                dist: Some(DistInfo {
                    dist_type: "path".into(),
                    url: format!("/tmp/{name}"),
                    reference: None,
                    shasum: None,
                    mirrors: None,
                }),
                require: BTreeMap::new(),
                require_dev: BTreeMap::new(),
                package_type: Some("library".into()),
                extra: None,
                autoload: Some(autoload),
                autoload_dev: None,
                notification_url: None,
                license: vec![],
                description: None,
                homepage: None,
                keywords: vec![],
                time: None,
                replace: BTreeMap::new(),
                provide: BTreeMap::new(),
                conflict: BTreeMap::new(),
                suggest: BTreeMap::new(),
                bin: vec![],
                abandoned: None,
                unknown: BTreeMap::new(),
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        std::fs::create_dir_all(vendor.join("acme/files")).unwrap();
        std::fs::create_dir_all(vendor.join("acme/handler/src")).unwrap();
        std::fs::write(
            vendor.join("acme/files/bootstrap.php"),
            r#"<?php
function safe_exec_stub(): string { return 'safe-ok'; }
"#,
        )
        .unwrap();
        std::fs::write(
            vendor.join("acme/handler/src/Handler.php"),
            r#"<?php
namespace Acme;
class Handler {
    public static function handle($event): void {
        file_put_contents(getcwd() . '/files-autoload.txt', \safe_exec_stub());
    }
}
"#,
        )
        .unwrap();

        let lock = ComposerLock {
            packages: vec![
                locked_pkg(
                    "acme/files",
                    AutoloadConfig {
                        files: vec!["bootstrap.php".into()],
                        ..Default::default()
                    },
                ),
                locked_pkg(
                    "acme/handler",
                    AutoloadConfig {
                        psr4: [(
                            "Acme\\".into(),
                            composer_core::PathOrPaths::One("src/".into()),
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    },
                ),
            ],
            ..Default::default()
        };
        let manifest: ComposerJson = serde_json::from_value(json!({ "name": "acme/app" })).unwrap();
        generate(
            root,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions::default(),
        )
        .unwrap();

        let m = manifest_with(json!({
            "post-autoload-dump": "Acme\\Handler::handle"
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, root, true).unwrap();
        let body = std::fs::read_to_string(root.join("files-autoload.txt")).unwrap();
        assert_eq!(body.trim(), "safe-ok");
    }

    #[test]
    fn callable_runner_exposes_root_package_extra() {
        if !php_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("composer.json"),
            r#"{
  "name": "acme/app",
  "extra": {
    "aws/aws-sdk-php": ["S3", "Sqs"]
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("Hooks.php"),
            r#"<?php
namespace Acme;
class Hooks {
    public static function postAutoloadDump($event) {
        $extra = $event->getComposer()->getPackage()->getExtra();
        file_put_contents(getcwd() . '/root-extra.json', json_encode($extra['aws/aws-sdk-php'] ?? []));
    }
}
"#,
        )
        .unwrap();
        std::fs::write(
            vendor.join("autoload.php"),
            "<?php\nrequire __DIR__ . '/../src/Hooks.php';\n",
        )
        .unwrap();

        let m = manifest_with(json!({
            "post-autoload-dump": "Acme\\Hooks::postAutoloadDump"
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, root, true).unwrap();
        let body = std::fs::read_to_string(root.join("root-extra.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, json!(["S3", "Sqs"]));
    }

    #[test]
    fn runs_php_callable_with_event_stub() {
        if !php_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let src = root.join("src");
        let vendor = root.join("vendor");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(
            src.join("Hooks.php"),
            r#"<?php
namespace Acme;
class Hooks {
    public static function postAutoloadDump($event) {
        $vendor = $event->getComposer()->getConfig()->get('vendor-dir');
        $name = $event->getName();
        $dev = $event->isDevMode() ? 'dev' : 'nodev';
        $io = $event->getIO();
        $io->write('hook-ok');
        file_put_contents(getcwd() . '/hook.txt', $name . "\n" . $vendor . "\n" . $dev . "\n");
    }
}
"#,
        )
        .unwrap();
        std::fs::write(
            vendor.join("autoload.php"),
            "<?php\nrequire __DIR__ . '/../src/Hooks.php';\n",
        )
        .unwrap();

        let m = manifest_with(json!({
            "post-autoload-dump": "Acme\\Hooks::postAutoloadDump"
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, root, true).unwrap();
        let body = std::fs::read_to_string(root.join("hook.txt")).unwrap();
        assert!(body.contains("post-autoload-dump"), "{body}");
        assert!(body.contains("vendor"), "{body}");
        assert!(body.contains("dev"), "{body}");
    }
}
