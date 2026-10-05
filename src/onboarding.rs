//! The block a fresh install prints until someone pairs with it.
//!
//! Installers are the wrong place for this: a host set up from a package, a
//! container image, or an unpacked tarball never runs one, and whatever the
//! script said scrolled past long ago. The server knows better than any of
//! them whether it has ever been paired, and what address it is reachable on,
//! so it says so itself — every boot until there is a device, then never
//! again.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use colored::Colorize;
use sqlx::SqlitePool;

use crate::storage::repositories::DeviceRepository;

/// Print the getting-started block if no device has ever paired.
///
/// Best-effort throughout: a failed device lookup means staying quiet rather
/// than nagging a working install, since the block is a convenience and the
/// query is not on any critical path.
pub async fn print_if_unpaired(pool: &SqlitePool, bind_addr: SocketAddr) {
    let devices = match DeviceRepository::new(pool.clone()).get_all().await {
        Ok(d) => d,
        Err(_) => return,
    };

    if devices.iter().any(|d| d.is_active) {
        return;
    }

    let urls = reachable_urls(bind_addr);

    println!();
    println!("  {}", "Getting started".bold().cyan());
    println!("  {}", "no devices paired yet".dimmed());
    println!();
    println!("  {}", "add this server in the Remon app:".dimmed());
    for url in &urls {
        println!("    {}", url.bold());
    }
    println!();
    println!("  {}", "then get a pairing code on this host:".dimmed());
    println!("    {}", pair_command().bold());
    println!();
}

/// The `pair` invocation that reaches this server's database.
fn pair_command() -> String {
    let paths = crate::paths::get();
    // SAFETY: geteuid takes no arguments and cannot fail.
    #[cfg(unix)]
    let sudo = if unsafe { libc::geteuid() } == 0 {
        "sudo "
    } else {
        ""
    };
    #[cfg(not(unix))]
    let sudo = "";
    format!(
        "{sudo}remon-server --config-dir {} --data-dir {} pair",
        paths.config_dir.display(),
        paths.data_dir.display()
    )
}

/// Addresses worth showing an operator. Binding a wildcard says nothing about
/// how to reach the host, so resolve the address it would actually use to
/// leave the machine and offer loopback alongside it.
fn reachable_urls(bind: SocketAddr) -> Vec<String> {
    let port = bind.port();
    let mut urls = Vec::new();

    if bind.ip().is_unspecified() {
        if let Some(ip) = primary_local_ip() {
            urls.push(format!("http://{ip}:{port}"));
        }
        urls.push(format!("http://127.0.0.1:{port}"));
    } else {
        urls.push(format!("http://{}:{}", bind.ip(), port));
    }

    urls
}

/// The local address the routing table would pick to reach the outside world.
///
/// A connected UDP socket only records a peer and performs the route lookup —
/// no packet leaves the host. The peer is TEST-NET-1, which exists to be a
/// destination nobody actually serves.
fn primary_local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80)).ok()?;
    let addr = socket.local_addr().ok()?;
    if addr.ip().is_unspecified() || addr.ip().is_loopback() {
        None
    } else {
        Some(addr.ip())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_bind_always_offers_loopback() {
        let urls = reachable_urls("0.0.0.0:8080".parse().unwrap());
        assert!(urls.iter().any(|u| u == "http://127.0.0.1:8080"));
    }

    #[test]
    fn an_explicit_bind_is_shown_as_configured() {
        let urls = reachable_urls("10.1.2.3:9000".parse().unwrap());
        assert_eq!(urls, vec!["http://10.1.2.3:9000".to_string()]);
    }
}
