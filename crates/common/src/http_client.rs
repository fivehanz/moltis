//! Shared HTTP client builder with optional upstream proxy.
//!
//! All crates that need a `reqwest::Client` with proxy support should use
//! [`build_http_client`] rather than building their own, to keep proxy
//! handling consistent.
//!
//! The gateway calls [`set_upstream_proxy`] once at startup so that any
//! crate can later retrieve the URL via [`upstream_proxy_url`] without
//! needing it threaded through every constructor.

static UPSTREAM_PROXY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Store the user-configured upstream proxy URL (call once at startup).
pub fn set_upstream_proxy(url: &str) {
    let _ = UPSTREAM_PROXY.set(url.to_string());
}

/// Return the upstream proxy URL, if one was configured.
pub fn upstream_proxy_url() -> Option<&'static str> {
    UPSTREAM_PROXY.get().map(String::as_str)
}

/// Build a [`reqwest::Client`] with optional upstream proxy.
///
/// When `proxy_url` is `Some`, all requests are routed through that proxy
/// (HTTP CONNECT for HTTPS targets, forward-proxy for plain HTTP).
/// Localhost/loopback addresses are automatically excluded via `no_proxy`.
///
/// Supports `http://`, `https://`, `socks5://`, and `socks5h://` schemes.
pub fn build_http_client(proxy_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(url) = proxy_url {
        match reqwest::Proxy::all(url) {
            Ok(proxy) => {
                let proxy =
                    proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
                builder = builder.proxy(proxy);
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to parse upstream proxy URL, falling back to direct connection"
                );
            },
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Build a [`reqwest::Client`] using the globally configured upstream proxy.
///
/// Convenience wrapper around [`build_http_client`] + [`upstream_proxy_url`].
pub fn build_default_http_client() -> reqwest::Client {
    build_http_client(upstream_proxy_url())
}

/// Apply the upstream proxy to an existing [`reqwest::ClientBuilder`].
///
/// Useful when callers need additional builder options (timeout, redirect
/// policy, etc.) but still want the global proxy applied.
pub fn apply_proxy(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if let Some(url) = upstream_proxy_url()
        && let Ok(proxy) = reqwest::Proxy::all(url)
    {
        let proxy = proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
        builder = builder.proxy(proxy);
    }
    builder
}

/// Redact credentials from a proxy URL for safe logging.
///
/// Returns a version of the URL with `user:pass@` replaced by `***@`.
/// If the URL cannot be parsed, returns the URL as-is (no secrets to leak).
pub fn redact_proxy_url(url: &str) -> String {
    // Parse minimally: look for `://user:pass@` pattern.
    // Use `rfind('@')` so that literal `@` characters inside a
    // percent-encoded password (e.g. `user:p%40ss@host`) are still
    // fully redacted — the last `@` is always the userinfo delimiter.
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.rfind('@') {
            // Has userinfo — redact it.
            let host_part = &after_scheme[at_pos..]; // includes '@'
            return format!("{}://***{host_part}", &url[..scheme_end]);
        }
    }
    // No credentials or unparseable — return as-is (no secrets to leak).
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_returns_default_client() {
        let client = build_http_client(None);
        // Smoke test: the client was created without error.
        drop(client);
    }

    #[test]
    fn valid_http_proxy() {
        let client = build_http_client(Some("http://127.0.0.1:8080"));
        drop(client);
    }

    #[test]
    fn valid_socks5_proxy() {
        let client = build_http_client(Some("socks5://127.0.0.1:1080"));
        drop(client);
    }

    #[test]
    fn invalid_proxy_url_falls_back() {
        // Garbage URL: should fall back to a plain client, not panic.
        let client = build_http_client(Some("not-a-url"));
        drop(client);
    }

    #[test]
    fn apply_proxy_with_none_is_passthrough() {
        // When no upstream proxy is set, apply_proxy is a no-op.
        let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10));
        let builder = apply_proxy(builder);
        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        drop(client);
    }

    #[test]
    fn redact_proxy_url_with_credentials() {
        assert_eq!(
            redact_proxy_url("http://user:p%40ss@proxy.example.com:8080"),
            "http://***@proxy.example.com:8080"
        );
    }

    #[test]
    fn redact_proxy_url_without_credentials() {
        assert_eq!(
            redact_proxy_url("http://proxy.example.com:8080"),
            "http://proxy.example.com:8080"
        );
    }

    #[test]
    fn redact_proxy_url_socks5_with_credentials() {
        assert_eq!(
            redact_proxy_url("socks5://admin:secret@10.0.0.1:1080"),
            "socks5://***@10.0.0.1:1080"
        );
    }

    #[test]
    fn redact_proxy_url_user_only() {
        // user without password still has '@'
        assert_eq!(
            redact_proxy_url("http://user@proxy:8080"),
            "http://***@proxy:8080"
        );
    }

    #[test]
    fn redact_proxy_url_at_in_password() {
        // Literal '@' in password (percent-encoded as %40 but decoded in URL).
        // rfind ensures we split on the last '@' (the userinfo delimiter).
        assert_eq!(
            redact_proxy_url("http://user:p@ss@proxy:8080"),
            "http://***@proxy:8080"
        );
    }
}
