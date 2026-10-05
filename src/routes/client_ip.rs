//! The client address behind any trusted reverse proxies.
//!
//! Each proxy appends the address it received the request from to
//! `X-Forwarded-For`, so only the rightmost `hops` entries were written by
//! proxies we trust. Everything left of them came from the client and is
//! ignored. `X-Real-IP` and `Forwarded` are not read: a proxy that does not
//! set them passes the client's own copy through.

use std::net::{IpAddr, SocketAddr};

use axum::{extract::ConnectInfo, http::HeaderMap};
use tower_governor::{GovernorError, key_extractor::KeyExtractor};

/// `hops` is the number of trusted proxies in front of the server; 0 means
/// none, and the TCP peer is the client.
pub fn client_ip(headers: &HeaderMap, peer: IpAddr, hops: usize) -> IpAddr {
    if hops == 0 {
        return peer;
    }
    // Repeated headers are one list, in order (RFC 9110 §5.3).
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // Fewer entries than hops: the leftmost is still one a proxy wrote.
    let Some(entry) = entries.get(entries.len().saturating_sub(hops)) else {
        return peer;
    };
    parse(entry).unwrap_or(peer)
}

/// Bare addresses, plus `1.2.3.4:5678` and `[::1]:5678` from proxies that
/// append the port.
fn parse(entry: &str) -> Option<IpAddr> {
    entry
        .parse::<IpAddr>()
        .or_else(|_| entry.parse::<SocketAddr>().map(|sa| sa.ip()))
        .ok()
}

/// Rate-limit key: the same address handlers see.
#[derive(Debug, Clone, Copy)]
pub struct ClientIpKeyExtractor {
    pub hops: usize,
}

impl KeyExtractor for ClientIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, GovernorError> {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.ip())
            .ok_or(GovernorError::UnableToExtractKey)?;
        Ok(client_ip(req.headers(), peer, self.hops))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn without_a_proxy_the_header_is_ignored() {
        assert_eq!(client_ip(&xff(&["9.9.9.9"]), PEER, 0), PEER);
    }

    #[test]
    fn a_spoofed_entry_left_of_the_proxy_is_ignored() {
        // nginx `proxy_add_x_forwarded_for` appends to what the client sent.
        let h = xff(&["6.6.6.6, 203.0.113.7"]);
        assert_eq!(client_ip(&h, PEER, 1), ip("203.0.113.7"));
    }

    #[test]
    fn two_proxies_count_two_from_the_right() {
        let h = xff(&["6.6.6.6, 203.0.113.7, 198.51.100.2"]);
        assert_eq!(client_ip(&h, PEER, 2), ip("203.0.113.7"));
    }

    #[test]
    fn repeated_headers_form_one_list() {
        let h = xff(&["6.6.6.6", "203.0.113.7"]);
        assert_eq!(client_ip(&h, PEER, 1), ip("203.0.113.7"));
    }

    #[test]
    fn fewer_entries_than_hops_take_the_leftmost() {
        assert_eq!(
            client_ip(&xff(&["203.0.113.7"]), PEER, 2),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn ports_are_dropped() {
        assert_eq!(
            client_ip(&xff(&["203.0.113.7:4711"]), PEER, 1),
            ip("203.0.113.7")
        );
        assert_eq!(
            client_ip(&xff(&["[2001:db8::1]:4711"]), PEER, 1),
            ip("2001:db8::1")
        );
    }

    #[test]
    fn a_missing_or_garbled_header_falls_back_to_the_peer() {
        assert_eq!(client_ip(&HeaderMap::new(), PEER, 1), PEER);
        assert_eq!(client_ip(&xff(&["unknown"]), PEER, 1), PEER);
    }
}
