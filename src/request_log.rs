//! Request-log helpers.
//!
//! SSE/WS browser clients pass their JWT in the `access_token` query
//! parameter, so the URI must be scrubbed before it reaches stdout or the
//! DB-backed log. Used by the request tracing span built in `build_router`.

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
}
