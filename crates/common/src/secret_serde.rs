//! Shared serde helpers for `Secret<String>` fields.
//!
//! Two serialization paths:
//! - **Storage** (`serialize_secret`, `serialize_option_secret`): exposes raw values for persistence.
//! - **Redacted** (`serialize_secret_redacted`, `serialize_option_secret_redacted`): writes
//!   `"[REDACTED]"` for API responses where secrets must not leak.
//!
//! The [`Redacted`] wrapper type lets channel config structs opt into the redacted path
//! without cloning or runtime string matching.

use secrecy::{ExposeSecret, Secret};

/// Sentinel value used for redacted secret fields in API responses.
pub const REDACTED: &str = "[REDACTED]";

// ---------------------------------------------------------------------------
// Storage path (exposes raw value)
// ---------------------------------------------------------------------------

/// Serialize a `Secret<String>` by exposing the raw value. Use for storage/persistence only.
pub fn serialize_secret<S: serde::Serializer>(
    secret: &Secret<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose_secret())
}

/// Serialize an `Option<Secret<String>>` by exposing the raw value (or `null`).
pub fn serialize_option_secret<S: serde::Serializer>(
    secret: &Option<Secret<String>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match secret {
        Some(s) => serializer.serialize_some(s.expose_secret()),
        None => serializer.serialize_none(),
    }
}

// ---------------------------------------------------------------------------
// Redacted path (writes "[REDACTED]")
// ---------------------------------------------------------------------------

/// Serialize a `Secret<String>` as `"[REDACTED]"`. Use for API responses.
pub fn serialize_secret_redacted<S: serde::Serializer>(
    _secret: &Secret<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(REDACTED)
}

/// Serialize an `Option<Secret<String>>` as `"[REDACTED]"` (Some) or `null` (None).
pub fn serialize_option_secret_redacted<S: serde::Serializer>(
    secret: &Option<Secret<String>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match secret {
        Some(_) => serializer.serialize_str(REDACTED),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use {super::*, serde::Serialize};

    #[derive(Serialize)]
    struct StorageExample {
        #[serde(serialize_with = "serialize_secret")]
        token: Secret<String>,
        #[serde(serialize_with = "serialize_option_secret")]
        optional: Option<Secret<String>>,
    }

    #[derive(Serialize)]
    struct RedactedExample {
        #[serde(serialize_with = "serialize_secret_redacted")]
        token: Secret<String>,
        #[serde(serialize_with = "serialize_option_secret_redacted")]
        optional: Option<Secret<String>>,
    }

    #[test]
    fn storage_path_exposes_values() {
        let ex = StorageExample {
            token: Secret::new("my-secret".into()),
            optional: Some(Secret::new("opt-secret".into())),
        };
        let v = serde_json::to_value(&ex).unwrap();
        assert_eq!(v["token"], "my-secret");
        assert_eq!(v["optional"], "opt-secret");
    }

    #[test]
    fn storage_path_option_none_is_null() {
        let ex = StorageExample {
            token: Secret::new("tok".into()),
            optional: None,
        };
        let v = serde_json::to_value(&ex).unwrap();
        assert!(v["optional"].is_null());
    }

    #[test]
    fn redacted_path_hides_values() {
        let ex = RedactedExample {
            token: Secret::new("my-secret".into()),
            optional: Some(Secret::new("opt-secret".into())),
        };
        let v = serde_json::to_value(&ex).unwrap();
        assert_eq!(v["token"], REDACTED);
        assert_eq!(v["optional"], REDACTED);
    }

    #[test]
    fn redacted_path_option_none_is_null() {
        let ex = RedactedExample {
            token: Secret::new("tok".into()),
            optional: None,
        };
        let v = serde_json::to_value(&ex).unwrap();
        assert!(v["optional"].is_null());
    }
}
