//! SSRF / private-range guard for webhook channel URLs.
//!
//! Three checkpoints use this:
//! - REST create / update handlers (`routes/rest/notifications.rs`) — early
//!   fail-fast at 400 before the row hits the DB.
//! - Channel send (`channels/webhook.rs::send`) — re-checked each send, for a
//!   readable error.
//! - [`GuardedResolver`], the notification client's DNS. It checks the
//!   addresses the connection actually uses, so a name that answers
//!   differently by the time reqwest resolves it (DNS rebinding) is still
//!   stopped. The two checks above resolve separately from reqwest.
//!
//! Default-deny: any IP in a private / loopback / link-local / ULA range is
//! rejected unless either the master switch is on or the hostname appears in
//! the allow-list.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

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
        self.allowed_private_hosts.contains(&host_lc)
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

/// DNS resolver for the notification HTTP client that refuses names
/// resolving into a blocked range, under the same policy as [`check_url`].
/// IP-literal URLs never reach a resolver; `check_url` covers those.
pub struct GuardedResolver {
    policy: WebhookPolicy,
}

impl GuardedResolver {
    pub fn new(policy: WebhookPolicy) -> Self {
        Self { policy }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let open = self.policy.allow_private_targets || self.policy.host_allow_listed(&host);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = lookup_host((host.as_str(), 0)).await?.collect();
            if !open && let Some(bad) = addrs.iter().find(|a| is_blocked_ip(&a.ip())) {
                return Err(format!(
                    "'{host}' resolves to {} which is in a blocked private/loopback range",
                    bad.ip()
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The outbound base URL a channel will connect to, for SSRF policy checks.
/// Returns `None` for channel types whose destination is fixed server-side
/// (FCM → Google) or validated per-subscriber elsewhere (web-push relay
/// endpoints, checked in the channel's send path and at subscribe time).
///
/// Shared by the REST create/update validators and the boot-time audit so a
/// new channel type only needs wiring in one place.
pub fn channel_check_url(channel_type: &str, config: &serde_json::Value) -> Option<String> {
    match channel_type {
        "webhook" => Some(config["url"].as_str().unwrap_or("").to_string()),
        "ntfy" => {
            // Mirrors NtfyChannel::new normalization: empty → public default.
            let server = config["server"].as_str().unwrap_or("").trim();
            let server = if server.is_empty() {
                "https://ntfy.sh"
            } else {
                server
            };
            Some(server.trim_end_matches('/').to_string())
        }
        _ => None,
    }
}

/// True if `ip` is in a range we never want a webhook to target.
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()              // 127.0.0.0/8
        || ip.is_private()        // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()     // 169.254/16 — incl. cloud metadata
        || ip.is_multicast()      // 224/4
        || ip.is_documentation() // 192.0.2 / 198.51.100 / 203.0.113
        || a == 0                 // 0/8, "this network"
        || (a == 100 && (b & 0xc0) == 64) // 100.64/10, CGNAT and Tailscale
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24, IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // 198.18/15, benchmarking
        || a >= 240 // 240/4 reserved, incl. 255.255.255.255
}

fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // An IPv4 address carried inside IPv6 gets the v4 check, so
    // `::ffff:127.0.0.1` or `64:ff9b::a00:1` cannot slip past it.
    if let Some(v4) = embedded_ipv4(ip) {
        return is_blocked_ipv4(&v4);
    }
    let seg = ip.segments();
    // Local-use NAT64 64:ff9b:1::/48, deprecated site-local fec0::/10 and
    // documentation 2001:db8::/32.
    if (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2] == 1)
        || (seg[0] & 0xffc0) == 0xfec0
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)
    {
        return true;
    }
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

/// The IPv4 address inside an IPv4-mapped (`::ffff:0:0/96`),
/// IPv4-compatible (`::/96`), NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`)
/// address.
fn embedded_ipv4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let seg = ip.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from(((hi as u32) << 16) | lo as u32);
    if seg[..6] == [0; 6] || seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(v4(seg[6], seg[7]));
    }
    if seg[0] == 0x2002 {
        return Some(v4(seg[1], seg[2]));
    }
    None
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
    fn blocks_shared_and_reserved_v4() {
        assert!(is_blocked_ip(&ip("100.64.0.1"))); // CGNAT
        assert!(is_blocked_ip(&ip("100.100.100.100"))); // Tailscale
        assert!(is_blocked_ip(&ip("100.127.255.255")));
        assert!(!is_blocked_ip(&ip("100.128.0.1")));
        assert!(is_blocked_ip(&ip("0.1.2.3")));
        assert!(is_blocked_ip(&ip("192.0.0.8")));
        assert!(is_blocked_ip(&ip("198.18.0.1")));
        assert!(is_blocked_ip(&ip("198.19.255.255")));
        assert!(!is_blocked_ip(&ip("198.20.0.1")));
        assert!(is_blocked_ip(&ip("240.0.0.1")));
    }

    #[test]
    fn blocks_v4_carried_in_v6() {
        assert!(is_blocked_ip(&ip("64:ff9b::7f00:1"))); // NAT64 127.0.0.1
        assert!(is_blocked_ip(&ip("64:ff9b::a9fe:a9fe"))); // NAT64 metadata
        assert!(!is_blocked_ip(&ip("64:ff9b::808:808"))); // NAT64 8.8.8.8
        assert!(is_blocked_ip(&ip("64:ff9b:1::1")));
        assert!(is_blocked_ip(&ip("2002:a00:1::1"))); // 6to4 10.0.0.1
        assert!(!is_blocked_ip(&ip("2002:808:808::1"))); // 6to4 8.8.8.8
        assert!(is_blocked_ip(&ip("::a00:1"))); // v4-compatible 10.0.0.1
        assert!(is_blocked_ip(&ip("fec0::1")));
        assert!(is_blocked_ip(&ip("2001:db8::1")));
    }

    #[tokio::test]
    async fn the_resolver_refuses_blocked_answers() {
        use reqwest::dns::Resolve;
        let guarded = GuardedResolver::new(policy(false, &[]));
        assert!(guarded.resolve("localhost".parse().unwrap()).await.is_err());

        let listed = GuardedResolver::new(policy(false, &["localhost"]));
        assert!(listed.resolve("localhost".parse().unwrap()).await.is_ok());
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
        assert!(check_url("http://mattermost.internal/x", &p).await.is_ok());
        // Different host still rejected.
        assert!(check_url("http://10.0.0.5/x", &p).await.is_err());
    }
}
