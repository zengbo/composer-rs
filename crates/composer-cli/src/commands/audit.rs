//! `composer-rs audit` — Packagist security advisories.

use super::{info, project_paths, success, warning};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::ComposerVersion;
use composer_lock::ComposerLock;
use serde::Deserialize;

#[derive(Args, Debug, Clone)]
pub struct AuditArgs {
    #[arg(long, default_value = "table")]
    pub format: String,

    #[arg(long)]
    pub no_dev: bool,
}

#[derive(Debug, Deserialize)]
struct AdvisoriesResponse {
    #[serde(default)]
    advisories: std::collections::BTreeMap<String, Vec<Advisory>>,
}

#[derive(Debug, Deserialize, Clone)]
struct Advisory {
    #[serde(default, rename = "advisoryId")]
    advisory_id: Option<String>,
    title: Option<String>,
    severity: Option<String>,
    cve: Option<Vec<String>>,
    #[serde(default, rename = "affectedVersions")]
    affected_versions: Option<String>,
    link: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct Finding {
    package: String,
    version: String,
    advisory: String,
    severity: String,
    cve: String,
    title: String,
    link: String,
}

pub async fn run(args: AuditArgs) -> Result<()> {
    let (_cwd, _json, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;
    let names: Vec<String> = lock
        .packages_to_install(with_dev)
        .into_iter()
        .map(|p| p.name.clone())
        .collect();

    if names.is_empty() {
        info("No packages to audit");
        return Ok(());
    }

    // Packagist security advisories API (batch by name).
    let client = reqwest::Client::new();
    let mut findings = Vec::new();

    // API: POST https://packagist.org/api/security-advisories/ with packages[]=
    // Also supports GET with query — use documented endpoint.
    for chunk in names.chunks(50) {
        // packagist expects packages[] array via query string
        let base = std::env::var("COMPOSER_RS_AUDIT_URL")
            .unwrap_or_else(|_| "https://packagist.org/api/security-advisories/".into());
        let mut url = reqwest::Url::parse(&base)?;
        {
            let mut qp = url.query_pairs_mut();
            for n in chunk {
                qp.append_pair("packages[]", n);
            }
        }
        let resp = client.get(url).send().await;
        let Ok(resp) = resp else {
            warning("Could not reach packagist security API");
            continue;
        };
        if !resp.status().is_success() {
            warning(&format!("security API HTTP {}", resp.status()));
            continue;
        }
        let body: AdvisoriesResponse = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                warning(&format!("parse advisories: {e}"));
                continue;
            }
        };
        for pkg in lock.packages_to_install(with_dev) {
            if let Some(advs) = body.advisories.get(&pkg.name) {
                let ver = ComposerVersion::parse(&pkg.version).ok();
                for adv in advs {
                    let affected = adv.affected_versions.as_deref().unwrap_or("*");
                    let hits = if let Some(v) = &ver {
                        version_in_affected(v, affected)
                    } else {
                        true
                    };
                    if hits {
                        findings.push(Finding {
                            package: pkg.name.clone(),
                            version: pkg.version.clone(),
                            advisory: adv.advisory_id.clone().unwrap_or_default(),
                            severity: adv.severity.clone().unwrap_or_else(|| "unknown".into()),
                            cve: adv.cve.as_ref().map(|c| c.join(", ")).unwrap_or_default(),
                            title: adv.title.clone().unwrap_or_default(),
                            link: adv.link.clone().unwrap_or_default(),
                        });
                    }
                }
            }
        }
    }

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else if findings.is_empty() {
        success("No known security advisories for locked packages");
    } else {
        println!(
            "{:<32} {:<12} {:<10} {:<20} {}",
            "Package", "Version", "Severity", "CVE", "Title"
        );
        for f in &findings {
            println!(
                "{:<32} {:<12} {:<10} {:<20} {}",
                f.package, f.version, f.severity, f.cve, f.title
            );
        }
    }

    if !findings.is_empty() {
        bail!("{} security advisory(ies) found", findings.len());
    }
    Ok(())
}

/// Match a locked version against Composer advisory `affectedVersions` text.
///
/// Supports `||` unions and comma-separated AND fragments (best-effort).
pub fn version_in_affected(ver: &ComposerVersion, affected: &str) -> bool {
    use composer_core::VersionConstraint;
    if affected.trim() == "*" || affected.is_empty() {
        return true;
    }
    for part in affected.split("||") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // convert commas to spaces for our constraint parser if needed
        let c = part.replace(',', " ");
        if VersionConstraint::new(c).matches(ver) {
            return true;
        }
    }
    // Fallback: substring raw version
    affected.contains(&ver.raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_caret_range_and_or() {
        let v = ComposerVersion::parse("1.2.3").unwrap();
        assert!(version_in_affected(&v, "^1.0"));
        assert!(version_in_affected(&v, ">=1.0,<1.3"));
        assert!(version_in_affected(&v, "^2.0 || ^1.2"));
        assert!(!version_in_affected(&v, "^2.0"));
    }

    #[test]
    fn star_matches_all() {
        let v = ComposerVersion::parse("9.9.9").unwrap();
        assert!(version_in_affected(&v, "*"));
    }
}
