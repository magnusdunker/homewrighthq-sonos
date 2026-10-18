//! Finding players: mDNS (`_sonos._tcp`) and SSDP (`ZonePlayer:1`). Both
//! only give an address; the player's device description says who it is,
//! and one player's zone group state names all the others.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::soap;

pub const SERVICE: &str = "_sonos._tcp.local.";
const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";
const SSDP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);

/// The room in an mDNS instance name like `RINCON_48A6B8...01400@Stue`.
pub fn room_of_instance(full_name: &str) -> Option<String> {
    let instance = full_name.strip_suffix(SERVICE).unwrap_or(full_name).trim_end_matches('.');
    instance.split_once('@').map(|(_, room)| room.to_string()).filter(|r| !r.is_empty())
}

/// The player's address from the LOCATION header of an SSDP answer.
pub fn ssdp_location(answer: &str) -> Option<String> {
    let mut is_sonos = false;
    let mut location = None;
    for line in answer.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "st" | "nt" if v.contains("ZonePlayer") => is_sonos = true,
            "server" if v.to_ascii_lowercase().contains("sonos") => is_sonos = true,
            "location" => location = crate::xml::addr_of_url(v.trim()),
            _ => {}
        }
    }
    location.filter(|_| is_sonos)
}

/// Browse mDNS until `found` is dropped; each resolved record sends its
/// IPv4 address with port 1400 (mDNS gives 1443, the HTTPS port).
pub fn browse(found: UnboundedSender<String>) -> Option<mdns_sd::ServiceDaemon> {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            crate::warn!("mDNS kan ikke starte: {e}");
            return None;
        }
    };
    let rx = match daemon.browse(SERVICE) {
        Ok(rx) => rx,
        Err(e) => {
            crate::warn!("mDNS-søgning fejlede: {e}");
            let _ = daemon.shutdown();
            return None;
        }
    };
    forward(rx, found);
    Some(daemon)
}

pub fn rebrowse(daemon: &mdns_sd::ServiceDaemon, found: UnboundedSender<String>) {
    let _ = daemon.stop_browse(SERVICE);
    match daemon.browse(SERVICE) {
        Ok(rx) => forward(rx, found),
        Err(e) => crate::warn!("mDNS-søgning fejlede: {e}"),
    }
}

fn forward(rx: mdns_sd::Receiver<mdns_sd::ServiceEvent>, found: UnboundedSender<String>) {
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv_async().await {
            match ev {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    let mut addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                    addrs.sort();
                    if let Some(a) = addrs.iter().find(|a| a.is_ipv4()) {
                        if found.send(soap::with_port(&a.to_string())).is_err() {
                            break;
                        }
                    }
                }
                mdns_sd::ServiceEvent::SearchStopped(_) => break,
                _ => {}
            }
        }
    });
}

/// The IPv4 addresses of this machine's network interfaces, to send SSDP
/// from each; the default route alone can be a VPN or a WSL switch.
fn interfaces() -> Vec<Ipv4Addr> {
    let mut out = vec![];
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let IpAddr::V4(a) = i.ip() {
            if !a.is_loopback() && !a.is_link_local() {
                out.push(a);
            }
        }
    }
    out
}

/// One SSDP search for `wait`; every player that answers is sent once.
pub async fn ssdp(wait: Duration, found: UnboundedSender<String>) {
    let query = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {SSDP_TARGET}\r\n\r\n");
    let mut sockets = vec![];
    for ip in interfaces() {
        let Ok(s) = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)) else { continue };
        let _ = s.set_multicast_if_v4(&ip);
        if s.bind(&SocketAddr::new(IpAddr::V4(ip), 0).into()).is_err() || s.set_nonblocking(true).is_err() {
            continue;
        }
        let Ok(s) = tokio::net::UdpSocket::from_std(s.into()) else { continue };
        sockets.push(s);
    }
    let mut seen = HashSet::new();
    let deadline = tokio::time::Instant::now() + wait;
    let tasks: Vec<_> = sockets
        .into_iter()
        .map(|s| {
            let query = query.clone();
            tokio::spawn(async move {
                let mut got = vec![];
                for _ in 0..2 {
                    let _ = s.send_to(query.as_bytes(), SSDP_ADDR).await;
                }
                let mut buf = vec![0u8; 2048];
                while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, s.recv_from(&mut buf)).await {
                    if let Some(a) = ssdp_location(&String::from_utf8_lossy(&buf[..n])) {
                        got.push(a);
                    }
                }
                got
            })
        })
        .collect();
    for t in tasks {
        for a in t.await.unwrap_or_default() {
            if seen.insert(a.clone()) && found.send(a).is_err() {
                return;
            }
        }
    }
}

/// Addresses that answer mDNS or SSDP within `wait`, for the setup form.
pub async fn scan(wait: Duration) -> Vec<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let daemon = browse(tx.clone());
    ssdp(wait, tx).await;
    if let Some(d) = daemon {
        let _ = d.shutdown();
    }
    let mut out = vec![];
    while let Ok(a) = rx.try_recv() {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// The local address a player at `addr` reaches us on, for event callbacks.
pub fn local_ip_towards(addr: &str) -> Option<IpAddr> {
    let target: SocketAddr = addr.parse().ok().or_else(|| std::net::ToSocketAddrs::to_socket_addrs(addr).ok()?.next())?;
    let bind = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(target).ok()?;
    Some(s.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names() {
        assert_eq!(room_of_instance("RINCON_48A6B8000001400@Stue._sonos._tcp.local.").as_deref(), Some("Stue"));
        assert_eq!(room_of_instance("Sonos-48A6B8000001._sonos._tcp.local."), None);
    }

    #[test]
    fn ssdp_answers() {
        let a = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age = 1800\r\nLOCATION: http://192.168.1.20:1400/xml/device_description.xml\r\nSERVER: Linux UPnP/1.0 Sonos/80.1-55014 (ZPS39)\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
        assert_eq!(ssdp_location(a).as_deref(), Some("192.168.1.20:1400"));
        let tv = "HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.30:9197/dmr\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(ssdp_location(tv), None);
    }

    #[test]
    fn local_route() {
        assert_eq!(local_ip_towards("127.0.0.1:1400"), Some("127.0.0.1".parse().unwrap()));
    }
}
