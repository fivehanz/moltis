//! Kimi-specific helpers for OAuth device flow and API requests.

use reqwest::header::HeaderMap;

use crate::config_dir::moltis_config_dir;

/// Get or generate a persistent device ID for Kimi API headers.
/// Stored at `~/.config/moltis/kimi_device_id`.
pub fn get_or_create_device_id() -> String {
    let path = moltis_config_dir().join("kimi_device_id");

    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let dir = path.parent().unwrap();
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, &id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    id
}

/// Build the `X-Msh-*` headers required by Kimi's OAuth and API endpoints.
pub fn kimi_headers() -> HeaderMap {
    let device_id = get_or_create_device_id();
    let mut headers = HeaderMap::new();
    headers.insert("X-Msh-Platform", "web".parse().unwrap());
    headers.insert("X-Msh-Version", "1.0.0".parse().unwrap());
    headers.insert("X-Msh-Device-Name", "moltis".parse().unwrap());
    headers.insert("X-Msh-Device-Model", "cli".parse().unwrap());
    headers.insert(
        "X-Msh-Os-Version",
        std::env::consts::OS
            .parse()
            .unwrap_or_else(|_| "unknown".parse().unwrap()),
    );
    headers.insert("X-Msh-Device-Id", device_id.parse().unwrap());
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_headers_contain_required_fields() {
        let headers = kimi_headers();
        assert!(headers.get("X-Msh-Platform").is_some());
        assert!(headers.get("X-Msh-Version").is_some());
        assert!(headers.get("X-Msh-Device-Name").is_some());
        assert!(headers.get("X-Msh-Device-Model").is_some());
        assert!(headers.get("X-Msh-Os-Version").is_some());
        assert!(headers.get("X-Msh-Device-Id").is_some());
    }

    #[test]
    fn device_id_is_stable() {
        let id1 = get_or_create_device_id();
        let id2 = get_or_create_device_id();
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }
}
