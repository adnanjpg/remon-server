//! SSRF / private-range guard for webhook channel URLs.
//!
//! Two checkpoints use this:
//! - REST create / update handlers (`routes/rest/notifications.rs`) — early
//!   fail-fast at 400 before the row hits the DB.
//! - Channel send (`channels/webhook.rs::send`) — re-resolve each invocation
//!   as a DNS-rebinding defense.
//!
//! Default-deny: any IP in a private / loopback / link-local / ULA range is
//! rejected unless either the master switch is on or the hostname appears in
//! the allow-list.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;
use tokio::net::lookup_host;

use crate::config::WebhookCredentials;

#[derive(Debug, Clone, Default)]
pub struct WebhookPolicy {
    pub allow_private_targets: bool,
    /// Pre-normalized (trimmed, lowercased, empty entries stripped).
    pub allowed_private_hosts: Vec<String>,
}

impl WebhookPolicy {
    pub fn from_credentials(c: &WebhookCredentials) -> Self {
        let allowed_private_hosts = c
            .allowed_private_hosts
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        Self {
            allow_private_targets: c.allow_private_targets,
            allowed_private_hosts,
        }
    }

    fn host_allow_listed(&self, host: &str) -> bool {
        let host_lc = host.to_ascii_lowercase();
        self.allowed_private_hosts.iter().any(|h| *h == host_lc)
    }
}

/// Validate `url` against the policy. `Ok(())` means the channel may proceed;
/// `Err(message)` should bubble up as a 400 at create-time or a send error at
/// send-time.
///
/// Fails closed on DNS errors — the safer side of a TOCTOU window.
pub async fn check_url(url: &str, policy: &WebhookPolicy) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| format!("invalid url '{}': {}", url, e))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "url scheme must be http or https, got '{}'",
            scheme
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;

    // Master override.
    if policy.allow_private_targets {
        return Ok(());
    }

    // Hostname allow-list bypass — checked before DNS so an operator who
    // explicitly trusts a name doesn't depend on resolver behavior.
    if policy.host_allow_listed(host) {
        return Ok(());
    }

    // IP-literal URL (`http://10.0.0.5/x` or `http://[::1]/y`).
    // `host_str()` strips IPv6 brackets, so direct IpAddr parse handles both.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(format!(
                "url host {} is in a blocked private/loopback range \
                 (set notifications.webhook.allow_private_targets=true \
                 or add the host to notifications.webhook.allowed_private_hosts to override)",
                ip
            ));
        }
        return Ok(());
    }

    // Hostname — DNS resolve. Port 0 is fine, resolvers accept it.
    let lookup_target = format!("{}:0", host);
    let addrs: Vec<std::net::SocketAddr> = lookup_host(&lookup_target)
        .await
        .map_err(|e| format!("dns lookup for '{}' failed: {}", host, e))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("dns lookup for '{}' returned no addresses", host));
    }

    for sa in &addrs {
        if is_blocked_ip(&sa.ip()) {
            return Err(format!(
                "url host '{}' resolves to {} which is in a blocked private/loopback range \
                 (set notifications.webhook.allow_private_targets=true \
                 or add '{}' to notifications.webhook.allowed_private_hosts to override)",
                host,
                sa.ip(),
                host
            ));
        }
    }

    Ok(())
}

/// True if `ip` is in a range we never want a webhook to target.
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()              // 127.0.0.0/8
        || ip.is_private()        // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()     // 169.254/16 — incl. cloud metadata
        || ip.is_unspecified()    // 0.0.0.0
        || ip.is_broadcast()      // 255.255.255.255
        || ip.is_multicast()      // 224/4
        || ip.is_documentation()  // 192.0.2 / 198.51.100 / 203.0.113
}

fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // IPv4-mapped IPv6 (`::ffff:x.x.x.x`) — defer to the v4 check to catch
    // `::ffff:127.0.0.1`-style attempts to slip past v4 logic.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    let seg = ip.segments();
    // Link-local fe80::/10 — first 10 bits 1111111010
    if (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // Unique local fc00::/7 — first 7 bits 1111110
    if (seg[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_v4_loopback_and_private() {
        assert!(is_blocked_ip(&ip("127.0.0.1")));
        assert!(is_blocked_ip(&ip("10.0.0.5")));
        assert!(is_blocked_ip(&ip("172.16.5.1")));
        assert!(is_blocked_ip(&ip("192.168.1.1")));
        assert!(is_blocked_ip(&ip("169.254.169.254"))); // cloud metadata
        assert!(is_blocked_ip(&ip("0.0.0.0")));
        assert!(is_blocked_ip(&ip("255.255.255.255")));
        assert!(is_blocked_ip(&ip("224.0.0.1"))); // multicast
        assert!(is_blocked_ip(&ip("192.0.2.1"))); // TEST-NET-1
    }

    #[test]
    fn allows_v4_public() {
        assert!(!is_blocked_ip(&ip("8.8.8.8")));
        assert!(!is_blocked_ip(&ip("1.1.1.1")));
        assert!(!is_blocked_ip(&ip("142.250.190.78")));
    }

    #[test]
    fn blocks_v6_loopback_and_local() {
        assert!(is_blocked_ip(&ip("::1")));
        assert!(is_blocked_ip(&ip("::")));
        assert!(is_blocked_ip(&ip("fe80::1")));
        assert!(is_blocked_ip(&ip("fc00::1")));
        assert!(is_blocked_ip(&ip("fd00::1")));
        assert!(is_blocked_ip(&ip("ff02::1"))); // multicast
    }

    #[test]
    fn blocks_v4_mapped_v6() {
        assert!(is_blocked_ip(&ip("::ffff:127.0.0.1")));
        assert!(is_blocked_ip(&ip("::ffff:10.0.0.1")));
        assert!(!is_blocked_ip(&ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn allows_v6_public() {
        assert!(!is_blocked_ip(&ip("2606:4700:4700::1111"))); // cloudflare
        assert!(!is_blocked_ip(&ip("2001:4860:4860::8888"))); // google
    }

    fn policy(allow_master: bool, hosts: &[&str]) -> WebhookPolicy {
        // Mirror the normalization done by `from_credentials` so tests
        // exercise the same shape the production code sees.
        let allowed_private_hosts = hosts
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        WebhookPolicy {
            allow_private_targets: allow_master,
            allowed_private_hosts,
        }
    }

    #[tokio::test]
    async fn rejects_loopback_literal() {
        let p = policy(false, &[]);
        assert!(check_url("http://127.0.0.1:9090/x", &p).await.is_err());
        assert!(check_url("http://[::1]/x", &p).await.is_err());
        assert!(check_url("http://10.0.5.20/admin", &p).await.is_err());
        assert!(check_url("http://169.254.169.254/", &p).await.is_err());
        assert!(check_url("http://[::ffff:127.0.0.1]/", &p).await.is_err());
    }

    #[tokio::test]
    async fn rejects_bad_scheme() {
        let p = policy(false, &[]);
        assert!(check_url("file:///etc/passwd", &p).await.is_err());
        assert!(check_url("gopher://x", &p).await.is_err());
        assert!(check_url("ftp://example.com", &p).await.is_err());
    }

    #[tokio::test]
    async fn master_switch_allows_private() {
        let p = policy(true, &[]);
        assert!(check_url("http://127.0.0.1/x", &p).await.is_ok());
        assert!(check_url("http://10.0.0.5/x", &p).await.is_ok());
        // Scheme still enforced.
        assert!(check_url("file:///x", &p).await.is_err());
    }

    #[tokio::test]
    async fn allow_list_bypasses_for_named_host() {
        let p = policy(false, &["metrics.lan", "Mattermost.Internal"]);
        // Allow-list match short-circuits before any DNS lookup, so even
        // if `metrics.lan` doesn't resolve in CI we accept it.
        assert!(check_url("http://metrics.lan/hook", &p).await.is_ok());
        // Case-insensitive.
        assert!(
            check_url("http://mattermost.internal/x", &p)
                .await
                .is_ok()
        );
        // Different host still rejected.
        assert!(check_url("http://10.0.0.5/x", &p).await.is_err());
    }
}
