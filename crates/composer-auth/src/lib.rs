//! Composer authentication: `auth.json` + `COMPOSER_AUTH`.

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;
use url::Url;

/// Credential store used for HTTP repository and dist downloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(rename = "http-basic", default)]
    pub http_basic: BTreeMap<String, HttpBasic>,
    #[serde(rename = "github-oauth", default)]
    pub github_oauth: BTreeMap<String, String>,
    /// Personal access token (string) or deploy/CI token (`{username, token}`).
    #[serde(rename = "gitlab-token", default)]
    pub gitlab_token: BTreeMap<String, GitlabToken>,
    #[serde(rename = "gitlab-oauth", default)]
    pub gitlab_oauth: BTreeMap<String, GitlabOauth>,
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

/// `auth.json` `gitlab-token.<host>`: a PAT string or deploy/CI object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitlabToken {
    /// `composer config gitlab-token.example.com <pat>`
    Personal(String),
    /// Deploy token / CI job token: `{ "username": "...", "token": "..." }`.
    Pair {
        username: String,
        #[serde(alias = "password")]
        token: String,
    },
}

/// `gitlab-oauth.<host>`: access token string or Composer refresh object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitlabOauth {
    Token(String),
    Refresh {
        token: String,
        #[serde(rename = "expires-at", default)]
        expires_at: Option<i64>,
        #[serde(rename = "refresh-token", default)]
        refresh_token: Option<String>,
    },
}

impl GitlabOauth {
    fn access_token(&self) -> &str {
        match self {
            Self::Token(t) => t,
            Self::Refresh { token, .. } => token,
        }
    }
}

/// Resolved auth for a single host.
#[derive(Debug, Clone)]
pub enum HostAuth {
    Basic { username: String, password: String },
    Bearer(String),
    TokenHeader { name: &'static str, value: String },
}

impl GitlabToken {
    /// Composer `GitLab::authorizeOAuth` + `AuthHelper::addAuthenticationOptions`.
    fn to_host_auth(&self) -> HostAuth {
        match self {
            Self::Personal(token) => HostAuth::TokenHeader {
                name: "PRIVATE-TOKEN",
                value: token.clone(),
            },
            Self::Pair { username, token } => {
                // Composer stores PAT as username=token, password=private-token.
                // If those two are reversed, it swaps them.
                let (identity, kind) = if is_gitlab_token_kind(username) {
                    (token.as_str(), username.as_str())
                } else if is_gitlab_token_kind(token) {
                    (username.as_str(), token.as_str())
                } else {
                    return HostAuth::Basic {
                        username: username.clone(),
                        password: token.clone(),
                    };
                };
                match kind {
                    "oauth2" => HostAuth::Bearer(identity.to_string()),
                    _ => HostAuth::TokenHeader {
                        name: "PRIVATE-TOKEN",
                        value: identity.to_string(),
                    },
                }
            }
        }
    }
}

fn is_gitlab_token_kind(s: &str) -> bool {
    matches!(s, "private-token" | "gitlab-ci-token" | "oauth2")
}

impl AuthStore {
    /// Load and merge: env `COMPOSER_AUTH` → project auth.json → Composer global auth.json.
    /// Later sources only fill keys not already set (env wins).
    pub fn load(project_root: Option<&Path>) -> Result<Self> {
        let mut store = AuthStore::default();

        if let Ok(raw) = std::env::var("COMPOSER_AUTH") {
            match serde_json::from_str::<AuthStore>(&raw) {
                Ok(env_store) => store.merge_prefer_self(env_store),
                Err(e) => warn!(error = %e, "COMPOSER_AUTH is not valid JSON"),
            }
        }

        if let Some(root) = project_root {
            let local = root.join("auth.json");
            merge_file(&mut store, &local);
        }

        let mut seen = std::collections::BTreeSet::new();
        for path in global_auth_candidates() {
            if !seen.insert(path.clone()) {
                continue;
            }
            merge_file(&mut store, &path);
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
        let bare = host.split(':').next().unwrap_or(host);
        // GitLab tokens first: Composer treats gitlab-token as the API credential
        // (PRIVATE-TOKEN / Bearer / deploy-token basic). http-basic is a fallback.
        if let Some(t) = lookup_host_map(&self.gitlab_token, host, bare) {
            return Some(t.to_host_auth());
        }
        if let Some(t) = lookup_host_map(&self.gitlab_oauth, host, bare) {
            return Some(HostAuth::Bearer(t.access_token().to_string()));
        }
        if let Some(b) = lookup_host_map(&self.http_basic, host, bare) {
            return Some(HostAuth::Basic {
                username: b.username.clone(),
                password: b.password.clone(),
            });
        }
        if let Some(t) = lookup_host_map(&self.bearer, host, bare) {
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

fn lookup_host_map<'a, T>(map: &'a BTreeMap<String, T>, host: &str, bare: &str) -> Option<&'a T> {
    map.get(host).or_else(|| map.get(bare)).or_else(|| {
        // auth.json keys sometimes include a scheme (rare but seen in the wild).
        map.get(&format!("https://{host}"))
            .or_else(|| map.get(&format!("https://{bare}")))
    })
}

fn merge_file(store: &mut AuthStore, path: &Path) {
    if !path.is_file() {
        return;
    }
    match AuthStore::from_file(path) {
        Ok(s) => store.merge_prefer_self(s),
        Err(e) => warn!(path = %path.display(), error = %e, "failed to parse auth.json"),
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

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
}

/// Composer home directory (`$COMPOSER_HOME`, else `~/.composer` if present, else XDG).
///
/// Matches `Composer\Factory::getComposerHome` — **not** macOS Application Support.
pub fn composer_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("COMPOSER_HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home));
        }
    }
    #[cfg(windows)]
    {
        return std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Composer"));
    }
    #[cfg(not(windows))]
    {
        let home = user_home()?;
        let legacy = home.join(".composer");
        if legacy.is_dir() {
            return Some(legacy);
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("composer"));
            }
        }
        Some(home.join(".config").join("composer"))
    }
}

/// Composer global auth.json path (`$COMPOSER_HOME/auth.json` or XDG / `~/.composer`).
pub fn global_auth_path() -> Option<PathBuf> {
    composer_home().map(|h| h.join("auth.json"))
}

/// All plausible Composer auth.json locations (first existing files are merged).
fn global_auth_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = global_auth_path() {
        out.push(p);
    }
    if let Some(home) = user_home() {
        out.push(home.join(".composer").join("auth.json"));
        out.push(home.join(".config").join("composer").join("auth.json"));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            out.push(PathBuf::from(xdg).join("composer").join("auth.json"));
        }
    }
    out
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

    #[test]
    fn gitlab_personal_token_sends_private_token_header() {
        let store =
            AuthStore::from_str(r#"{"gitlab-token":{"gitlab.rightcapital.io":"glpat-secret"}}"#)
                .unwrap();
        match store.for_url(
            "https://gitlab.rightcapital.io/api/v4/projects/274/packages/composer/archives/pkg.zip?sha=abc",
        ) {
            Some(HostAuth::TokenHeader { name, value }) => {
                assert_eq!(name, "PRIVATE-TOKEN");
                assert_eq!(value, "glpat-secret");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn gitlab_deploy_token_object_is_http_basic() {
        let store = AuthStore::from_str(
            r#"{
                "gitlab-token": {
                    "gitlab.example.com": {
                        "username": "gitlab+deploy-token-1",
                        "token": "gldt-xxx"
                    }
                }
            }"#,
        )
        .unwrap();
        match store.for_host("gitlab.example.com") {
            Some(HostAuth::Basic { username, password }) => {
                assert_eq!(username, "gitlab+deploy-token-1");
                assert_eq!(password, "gldt-xxx");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn gitlab_ci_job_token_object_is_private_token() {
        let store = AuthStore::from_str(
            r#"{
                "gitlab-token": {
                    "gitlab.example.com": {
                        "username": "gitlab-ci-token",
                        "token": "ci-job"
                    }
                }
            }"#,
        )
        .unwrap();
        match store.for_host("gitlab.example.com") {
            Some(HostAuth::TokenHeader { name, value }) => {
                assert_eq!(name, "PRIVATE-TOKEN");
                assert_eq!(value, "ci-job");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn gitlab_oauth_object_is_bearer() {
        let store = AuthStore::from_str(
            r#"{
                "gitlab-oauth": {
                    "gitlab.example.com": {
                        "token": "oauth-access",
                        "expires-at": 9999999999,
                        "refresh-token": "r"
                    }
                }
            }"#,
        )
        .unwrap();
        match store.for_host("gitlab.example.com") {
            Some(HostAuth::Bearer(t)) => assert_eq!(t, "oauth-access"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn mixed_gitlab_object_does_not_drop_http_basic() {
        let store = AuthStore::from_str(
            r#"{
                "http-basic": {"packagist.org": {"username": "u", "password": "p"}},
                "gitlab-token": {
                    "gitlab.example.com": {"username": "gitlab+deploy-token-1", "token": "t"}
                }
            }"#,
        )
        .unwrap();
        assert!(store.for_host("packagist.org").is_some());
        assert!(store.for_host("gitlab.example.com").is_some());
    }

    #[test]
    fn gitlab_token_wins_over_http_basic_on_same_host() {
        let store = AuthStore::from_str(
            r#"{
                "http-basic": {"gitlab.example.com": {"username": "u", "password": "p"}},
                "gitlab-token": {"gitlab.example.com": "glpat-preferred"}
            }"#,
        )
        .unwrap();
        match store.for_host("gitlab.example.com") {
            Some(HostAuth::TokenHeader { value, .. }) => assert_eq!(value, "glpat-preferred"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
