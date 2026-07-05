//! Request-log helpers.
//!
//! Two credentials travel inside request URIs and must be scrubbed before
//! the URI reaches stdout or the DB-backed log: the `access_token` query
//! parameter (SSE/WS browser clients) and the heartbeat capability slug,
//! which IS the path segment after `/ping/`. Used by the request tracing
//! span built in `build_app`.

/// Scrub every URI-borne credential before logging.
pub fn redact_uri(uri: &str) -> String {
    redact_access_token(&redact_ping_slug(uri))
}

/// Redact the capability slug in `/ping/{slug}...` paths. The suffix
/// (`/fail`, `/pause?...`) survives — it's the slug that must not land in
/// the 30-day request log.
fn redact_ping_slug(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("/ping/") else {
        return uri.to_string();
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    format!("/ping/REDACTED{}", &rest[end..])
}

/// Redact `access_token` query values before request URIs are logged.
pub fn redact_access_token(uri: &str) -> String {
    let Some((path, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let scrubbed: Vec<String> = query
        .split('&')
        .map(|pair| {
            if pair.starts_with("access_token=") || pair == "access_token" {
                "access_token=REDACTED".to_string()
            } else {
                pair.to_string()
            }
        })
        .collect();
    format!("{}?{}", path, scrubbed.join("&"))
}

#[cfg(test)]
mod tests {
    use super::redact_access_token;

    #[test]
    fn no_query_passes_through() {
        assert_eq!(redact_access_token("/sse/stats"), "/sse/stats");
    }

    #[test]
    fn token_first_param_redacted() {
        assert_eq!(
            redact_access_token("/sse/stats?access_token=abc.def.ghi"),
            "/sse/stats?access_token=REDACTED"
        );
    }

    #[test]
    fn token_with_other_params_redacted() {
        assert_eq!(
            redact_access_token("/sse/stats?foo=1&access_token=abc&bar=2"),
            "/sse/stats?foo=1&access_token=REDACTED&bar=2"
        );
    }

    #[test]
    fn unrelated_query_untouched() {
        assert_eq!(
            redact_access_token("/processes?limit=10"),
            "/processes?limit=10"
        );
    }

    #[test]
    fn ping_slug_redacted() {
        use super::redact_uri;
        assert_eq!(
            redact_uri("/ping/0123456789abcdef0123456789abcdef"),
            "/ping/REDACTED"
        );
        assert_eq!(
            redact_uri("/ping/0123456789abcdef0123456789abcdef/fail"),
            "/ping/REDACTED/fail"
        );
        assert_eq!(
            redact_uri("/ping/0123456789abcdef0123456789abcdef/pause?duration=3h&reason=deploy"),
            "/ping/REDACTED/pause?duration=3h&reason=deploy"
        );
        // Non-ping paths pass through untouched.
        assert_eq!(redact_uri("/pingx/abc"), "/pingx/abc");
        assert_eq!(redact_uri("/heartbeats/3/pings"), "/heartbeats/3/pings");
    }
}
