//! Composer `content-hash` — byte-exact with PHP `Locker::getContentHash`.

use composer_core::error::{Error, Result};
use composer_php_json::{Mode, encode};
use serde_json::{Map, Value};

/// Keys that participate in Composer's content-hash, in the order PHP's
/// `array_intersect($relevantKeys, array_keys($content))` would produce.
/// Order does not affect the hash (we `ksort` before encoding).
const RELEVANT_KEYS: &[&str] = &[
    "name",
    "version",
    "require",
    "require-dev",
    "conflict",
    "replace",
    "provide",
    "minimum-stability",
    "prefer-stable",
    "repositories",
    "extra",
];

/// Compute Composer's `content-hash` for a `composer.json` byte stream.
///
/// Algorithm (verbatim from `Locker::getContentHash`):
///
/// 1. JSON-decode the composer.json bytes.
/// 2. Pick [`RELEVANT_KEYS`] plus `config.platform` if present.
/// 3. `ksort` the resulting top-level keys alphabetically.
/// 4. PHP `json_encode(..., 0)` via [`composer_php_json::Mode::Hash`].
/// 5. MD5 hex.
pub fn content_hash(composer_json_bytes: &[u8]) -> Result<String> {
    let parsed: Value = serde_json::from_slice(composer_json_bytes)?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| Error::Manifest("composer.json top level must be a JSON object".into()))?;

    let mut relevant: Map<String, Value> = Map::new();
    for key in RELEVANT_KEYS {
        if let Some(v) = obj.get(*key) {
            relevant.insert((*key).to_string(), v.clone());
        }
    }
    if let Some(platform) = obj
        .get("config")
        .and_then(Value::as_object)
        .and_then(|c| c.get("platform"))
    {
        let mut config_subset = Map::new();
        config_subset.insert("platform".to_string(), platform.clone());
        relevant.insert("config".to_string(), Value::Object(config_subset));
    }

    sort_top_level(&mut relevant);

    let bytes = encode(&Value::Object(relevant), Mode::Hash);
    Ok(format!("{:x}", md5::compute(bytes)))
}

/// In-place ksort of an object's top-level keys (lexicographic on bytes,
/// matching PHP's default `ksort` for string keys). Nested objects keep
/// their own order — Composer's algorithm only sorts the top level.
fn sort_top_level(m: &mut Map<String, Value>) {
    let mut keys: Vec<String> = m.keys().cloned().collect();
    keys.sort();
    let mut rebuilt: Map<String, Value> = Map::new();
    for k in keys {
        let v = m.shift_remove(&k).expect("key came from m.keys()");
        rebuilt.insert(k, v);
    }
    *m = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_COMPOSER_JSON: &str = r#"{
    "name": "acme/widget-tool",
    "description": "ignored for hash",
    "minimum-stability": "stable",
    "prefer-stable": true,
    "require": {
        "php": "^8.3",
        "monolog/monolog": "^3.5",
        "ext-redis": "*"
    },
    "require-dev": {
        "phpunit/phpunit": "^10.5"
    },
    "extra": {
        "branch-alias": {
            "dev-main": "1.0.x-dev"
        }
    },
    "config": {
        "platform": {
            "php": "8.3.12"
        },
        "sort-packages": true
    }
}"#;

    const FIXTURE_EXPECTED_HASH: &str = "9b37bf1b84c6c80e4dae34a4a6a8c18d";

    const FIXTURE_EXPECTED_ENCODED: &str = concat!(
        r#"{"config":{"platform":{"php":"8.3.12"}},"#,
        r#""extra":{"branch-alias":{"dev-main":"1.0.x-dev"}},"#,
        r#""minimum-stability":"stable","#,
        r#""name":"acme\/widget-tool","#,
        r#""prefer-stable":true,"#,
        r#""require":{"php":"^8.3","monolog\/monolog":"^3.5","ext-redis":"*"},"#,
        r#""require-dev":{"phpunit\/phpunit":"^10.5"}}"#,
    );

    #[test]
    fn fixture_hash_matches_real_php() {
        let actual = content_hash(FIXTURE_COMPOSER_JSON.as_bytes()).unwrap();
        assert_eq!(actual, FIXTURE_EXPECTED_HASH);
    }

    #[test]
    fn fixture_encoded_bytes_match_real_php() {
        let parsed: Value = serde_json::from_str(FIXTURE_COMPOSER_JSON).unwrap();
        let obj = parsed.as_object().unwrap();
        let mut relevant: Map<String, Value> = Map::new();
        for key in RELEVANT_KEYS {
            if let Some(v) = obj.get(*key) {
                relevant.insert((*key).to_string(), v.clone());
            }
        }
        if let Some(platform) = obj
            .get("config")
            .and_then(Value::as_object)
            .and_then(|c| c.get("platform"))
        {
            let mut config_subset = Map::new();
            config_subset.insert("platform".to_string(), platform.clone());
            relevant.insert("config".to_string(), Value::Object(config_subset));
        }
        sort_top_level(&mut relevant);
        let bytes = encode(&Value::Object(relevant), Mode::Hash);
        assert_eq!(String::from_utf8(bytes).unwrap(), FIXTURE_EXPECTED_ENCODED);
    }

    #[test]
    fn require_key_order_preserved_from_source() {
        // Nested keys must stay in composer.json order, not alphabetical.
        let json = r#"{"require":{"symfony/console":"^6.0","php":"^8.1"}}"#;
        let hash_symfony_first = content_hash(json.as_bytes()).unwrap();

        let json_reordered = r#"{"require":{"php":"^8.1","symfony/console":"^6.0"}}"#;
        let hash_php_first = content_hash(json_reordered.as_bytes()).unwrap();

        assert_ne!(hash_symfony_first, hash_php_first);
    }

    #[test]
    fn missing_relevant_keys_hash_empty_object() {
        let bytes = br#"{"authors": [], "description": "x"}"#;
        let h = content_hash(bytes).unwrap();
        assert_eq!(h, "99914b932bd37a50b983c5e7c90ae93b");
    }

    #[test]
    fn config_keys_other_than_platform_are_ignored() {
        let base = br#"{"name":"a/b"}"#;
        let with_config =
            br#"{"name":"a/b","config":{"sort-packages":true,"optimize-autoloader":false}}"#;
        assert_eq!(
            content_hash(base).unwrap(),
            content_hash(with_config).unwrap()
        );
    }

    #[test]
    fn config_platform_participates() {
        let without = br#"{"name":"a/b"}"#;
        let with = br#"{"name":"a/b","config":{"platform":{"php":"8.2.0"}}}"#;
        assert_ne!(content_hash(without).unwrap(), content_hash(with).unwrap());
    }

    #[test]
    fn top_level_must_be_object() {
        let err = content_hash(b"[]").unwrap_err();
        assert!(err.to_string().contains("JSON object"));
    }
}
