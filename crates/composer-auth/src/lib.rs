//! Composer authentication: `auth.json` + `COMPOSER_AUTH`.

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

/// Credential store used for HTTP repository and dist downloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(rename = "http-basic", default)]
    pub http_basic: BTreeMap<String, HttpBasic>,
    #[serde(rename = "github-oauth", default)]
    pub github_oauth: BTreeMap<String, String>,
    #[serde(rename = "gitlab-token", default)]
    pub gitlab_token: BTreeMap<String, String>,
    #[serde(rename = "gitlab-oauth", default)]
    pub gitlab_oauth: BTreeMap<String, String>,
    #[serde(rename = "bearer", default)]
    pub bearer: BTreeMap<String, String>,
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpBasic {
    pub username: String,
    pub password: String,
}

/// Resolved auth for a single host.
#[derive(Debug, Clone)]
pub enum HostAuth {
    Basic { username: String, password: String },
    Bearer(String),
    TokenHeader { name: &'static str, value: String },
}

impl AuthStore {
    /// Load and merge: env `COMPOSER_AUTH` → project auth.json → global auth.json.
    /// Later sources only fill keys not already set (env wins).
    pub fn load(project_root: Option<&Path>) -> Result<Self> {
        let mut store = AuthStore::default();

        if let Ok(raw) = std::env::var("COMPOSER_AUTH") {
            if let Ok(env_store) = serde_json::from_str::<AuthStore>(&raw) {
                store.merge_prefer_self(env_store);
            }
        }

        if let Some(root) = project_root {
            let local = root.join("auth.json");
            if local.is_file() {
                if let Ok(s) = Self::from_file(&local) {
                    store.merge_prefer_self(s);
                }
            }
        }

        if let Some(global) = global_auth_path() {
            if global.is_file() {
                if let Ok(s) = Self::from_file(&global) {
                    store.merge_prefer_self(s);
                }
            }
        }

        Ok(store)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        serde_json::from_str(&text).map_err(|e| Error::other(format!("auth.json: {e}")))
    }

    pub fn from_str(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|e| Error::other(format!("auth json: {e}")))
    }

    /// Merge `other` into self; existing keys in self win.
    pub fn merge_prefer_self(&mut self, other: AuthStore) {
        for (k, v) in other.http_basic {
            self.http_basic.entry(k).or_insert(v);
        }
        for (k, v) in other.github_oauth {
            self.github_oauth.entry(k).or_insert(v);
        }
        for (k, v) in other.gitlab_token {
            self.gitlab_token.entry(k).or_insert(v);
        }
        for (k, v) in other.gitlab_oauth {
            self.gitlab_oauth.entry(k).or_insert(v);
        }
        for (k, v) in other.bearer {
            self.bearer.entry(k).or_insert(v);
        }
        for (k, v) in other.rest {
            self.rest.entry(k).or_insert(v);
        }
    }

    /// Lookup credentials for a URL (matches host, with/without port).
    pub fn for_url(&self, url: &str) -> Option<HostAuth> {
        let host = host_key(url)?;
        self.for_host(&host)
    }

    pub fn for_host(&self, host: &str) -> Option<HostAuth> {
        if let Some(b) = self.http_basic.get(host) {
            return Some(HostAuth::Basic {
                username: b.username.clone(),
                password: b.password.clone(),
            });
        }
        // try without port
        let bare = host.split(':').next().unwrap_or(host);
        if bare != host {
            if let Some(b) = self.http_basic.get(bare) {
                return Some(HostAuth::Basic {
                    username: b.username.clone(),
                    password: b.password.clone(),
                });
            }
        }
        if let Some(t) = self.bearer.get(host).or_else(|| self.bearer.get(bare)) {
            return Some(HostAuth::Bearer(t.clone()));
        }
        if let Some(t) = self
            .github_oauth
            .get(host)
            .or_else(|| self.github_oauth.get(bare))
            .or_else(|| self.github_oauth.get("github.com"))
        {
            if host.contains("github") || bare == "api.github.com" || bare == "github.com" {
                return Some(HostAuth::TokenHeader {
                    name: "Authorization",
                    value: format!("token {t}"),
                });
            }
        }
        if let Some(t) = self
            .gitlab_token
            .get(host)
            .or_else(|| self.gitlab_token.get(bare))
        {
            return Some(HostAuth::TokenHeader {
                name: "PRIVATE-TOKEN",
                value: t.clone(),
            });
        }
        if let Some(t) = self
            .gitlab_oauth
            .get(host)
            .or_else(|| self.gitlab_oauth.get(bare))
        {
            return Some(HostAuth::Bearer(t.clone()));
        }
        None
    }

    /// Apply auth headers / basic auth to a reqwest request builder.
    pub fn apply_to_request(
        &self,
        url: &str,
        builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match self.for_url(url) {
            Some(HostAuth::Basic { username, password }) => {
                builder.basic_auth(username, Some(password))
            }
            Some(HostAuth::Bearer(token)) => builder.bearer_auth(token),
            Some(HostAuth::TokenHeader { name, value }) => builder.header(name, value),
            None => builder,
        }
    }
}

fn host_key(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let host = u.host_str()?;
    match u.port() {
        Some(p) => Some(format!("{host}:{p}")),
        None => Some(host.to_string()),
    }
}

/// Composer global auth.json path (`$COMPOSER_HOME/auth.json` or XDG).
pub fn global_auth_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("COMPOSER_HOME") {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    directories::BaseDirs::new().map(|d| d.config_dir().join("composer").join("auth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_http_basic_and_looks_up_host() {
        let raw = r#"{
            "http-basic": {
                "repo.example.com": {
                    "username": "u",
                    "password": "p"
                }
            }
        }"#;
        let store = AuthStore::from_str(raw).unwrap();
        match store.for_url("https://repo.example.com/p2/foo.json") {
            Some(HostAuth::Basic { username, password }) => {
                assert_eq!(username, "u");
                assert_eq!(password, "p");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn env_wins_over_file_merge() {
        let mut a =
            AuthStore::from_str(r#"{"http-basic":{"h":{"username":"env","password":"e"}}}"#)
                .unwrap();
        let b = AuthStore::from_str(
            r#"{"http-basic":{"h":{"username":"file","password":"f"},"other":{"username":"o","password":"o"}}}"#,
        )
        .unwrap();
        a.merge_prefer_self(b);
        match a.for_host("h") {
            Some(HostAuth::Basic { username, .. }) => assert_eq!(username, "env"),
            _ => panic!("missing h"),
        }
        assert!(a.for_host("other").is_some());
    }
}
