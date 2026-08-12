//! Minimal Composer scripts runner (`scripts` in composer.json).

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_manifest::ComposerJson;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
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

fn run_named(
    scripts: &BTreeMap<String, Value>,
    name: &str,
    project_root: &Path,
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
        if let Some(ref_name) = cmd.strip_prefix('@') {
            // @php ... is not a script reference
            if ref_name.starts_with("php ") || ref_name == "php" {
                run_shell_line(
                    cmd,
                    project_root,
                    if is_last { extra_args } else { &[] },
                    dev_mode,
                )?;
            } else {
                // @script-name or @script-name arg1
                let mut parts = ref_name.split_whitespace();
                let ref_script = parts.next().unwrap_or("");
                let mut ref_args: Vec<String> = parts.map(|s| s.to_string()).collect();
                if is_last {
                    ref_args.extend(extra_args.iter().cloned());
                }
                run_named(
                    scripts,
                    ref_script,
                    project_root,
                    &ref_args,
                    dev_mode,
                    stack,
                )?;
            }
        } else {
            run_shell_line(
                cmd,
                project_root,
                if is_last { extra_args } else { &[] },
                dev_mode,
            )?;
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

fn run_shell_line(
    line: &str,
    project_root: &Path,
    extra_args: &[String],
    dev_mode: bool,
) -> Result<()> {
    let mut full = line.to_string();
    for a in extra_args {
        full.push(' ');
        full.push_str(a);
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
    cmd.current_dir(project_root);
    cmd.env("COMPOSER_DEV_MODE", if dev_mode { "1" } else { "0" });
    if let Ok(bin) = std::env::current_exe() {
        cmd.env("COMPOSER", bin);
    }
    cmd.env("COMPOSER_BINARY", "composer-rs");

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
    fn runs_echo_script() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manifest_with(json!({
            "post-autoload-dump": "echo ok-from-script > ran.txt"
        }));
        run_event(&m, ScriptEvent::PostAutoloadDump, tmp.path(), true).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("ran.txt")).unwrap();
        assert!(body.contains("ok-from-script"));
    }
}
