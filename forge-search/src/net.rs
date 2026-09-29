// SPDX-License-Identifier: Apache-2.0
//! Which addresses a crawler is allowed to reach.
//!
//! A crawler follows links written by other people, so the set of addresses it
//! may be pointed at is not the set its operator typed. A page can redirect it
//! at `127.0.0.1`, and without this the response comes back, gets indexed, and
//! is read out to the model — the user's own machine read aloud by a document
//! it fetched from someone else. Nothing about that needs the model's help.
//!
//! So: public addresses only. Loopback, the private ranges, link-local (which
//! is where cloud instance metadata lives, at `169.254.169.254`), carrier NAT
//! and the rest are refused.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Whether one address is on the public internet.
///
/// Pure, so it can be tested without a resolver. The interesting cases are the
/// ones that do not look like addresses at all — see `is_routable_public`.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: &Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_loopback()            // 127/8
        || ip.is_private()        // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()     // 169.254/16 — cloud instance metadata
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()    // 0.0.0.0, which many stacks route to local
        || a == 0                 // 0/8 "this network"
        || (a == 100 && (64..128).contains(&b)) // 100.64/10 carrier NAT
        || (a == 192 && b == 0)   // 192.0.0/24 protocol assignments
        || (a == 198 && (18..20).contains(&b)) // 198.18/15 benchmarking
        || ip.is_multicast()
        || a >= 240)              // 240/4 reserved, includes 255.255.255.255
}

fn is_public_v6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    // An IPv4 address wearing an IPv6 coat is still that address. `::ffff:127.0.0.1`
    // reaches loopback exactly as `127.0.0.1` does.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(&v4);
    }
    if let Some(v4) = ip.to_ipv4() {
        return is_public_v4(&v4);
    }
    let seg = ip.segments();
    let unique_local = (seg[0] & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (seg[0] & 0xffc0) == 0xfe80; // fe80::/10
    !(unique_local || link_local)
}

/// Whether a host is *plainly* local, judged without asking a resolver.
///
/// Literal addresses and the names every system special-cases. This is the
/// check the crawler uses, and it deliberately does no I/O: the crawler is
/// synchronous, runs inside a time budget, and a DNS lookup per URL would be
/// both a surprise and a way to stall a crawl on a hostile nameserver.
///
/// It is enough for the attack that matters. A redirect aimed at the local
/// machine names it — `http://127.0.0.1:2375/`, `http://[::1]/`,
/// `http://169.254.169.254/` — because the attacker needs the request to
/// arrive somewhere specific. A hostname that merely *resolves* to a private
/// address is caught one layer out, by `is_routable_public`, where a lookup is
/// happening anyway.
pub fn is_obviously_local(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    if let Ok(ip) = bare.parse::<IpAddr>() {
        return !is_public_ip(&ip);
    }
    // The same address written so it does not look like one. `2130706433`,
    // `127.1`, `0x7f.1` and `017700000001` are all 127.0.0.1 to the resolver
    // a connection actually goes through, and none of them parse as an
    // address here — Rust's parser wants four decimal octets. Forge's own URL
    // parser does not normalise them either, so the host reached this check
    // spelled exactly as written and walked past it, and the HTTP client
    // normalised it afterwards and connected to loopback.
    if let Some(ip) = parse_relaxed_ipv4(bare) {
        return !is_public_v4(&ip);
    }

    // Names, case-insensitively. `localhost` is required to be loopback;
    // `.local` is mDNS and `.internal` is the conventional private zone.
    let lower = bare.trim_end_matches('.').to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "broadcasthost"
}

/// The historical `inet_aton` spellings of an IPv4 address.
///
/// One to four parts, each decimal, octal (leading zero) or hex (`0x`), with
/// the last part filling the remaining octets. This is what a C resolver has
/// always accepted and what every browser still accepts, so it is what an
/// address means in practice regardless of what the strict parser says.
///
/// Returns `None` for anything that is not unambiguously one of these, so an
/// ordinary hostname is never mistaken for an address.
fn parse_relaxed_ipv4(host: &str) -> Option<Ipv4Addr> {
    if host.is_empty() || host.ends_with('.') {
        return None;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }

    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let lower = part.to_ascii_lowercase();
        let value = if let Some(hex) = lower.strip_prefix("0x") {
            if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            u64::from_str_radix(hex, 16).ok()?
        } else if lower.len() > 1 && lower.starts_with('0') {
            if !lower[1..].chars().all(|c| ('0'..='7').contains(&c)) {
                return None;
            }
            u64::from_str_radix(&lower[1..], 8).ok()?
        } else {
            if !lower.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            lower.parse::<u64>().ok()?
        };
        values.push(value);
    }

    // Every part but the last is one octet; the last fills what remains.
    let last = *values.last()?;
    let leading = &values[..values.len() - 1];
    if leading.iter().any(|&v| v > 255) {
        return None;
    }
    let remaining_octets = 4 - leading.len();
    let max_last = match remaining_octets {
        1 => 0xff,
        2 => 0xffff,
        3 => 0xff_ffff,
        _ => 0xffff_ffff,
    };
    if last > max_last {
        return None;
    }

    let mut bits: u32 = 0;
    for (i, &v) in leading.iter().enumerate() {
        bits |= (v as u32) << (8 * (3 - i));
    }
    bits |= last as u32;
    Some(Ipv4Addr::from(bits))
}

/// Whether a host names something on the public internet.
///
/// Resolves rather than matching strings, because the same machine has many
/// spellings. `127.1`, `2130706433`, `0x7f.1`, `::ffff:127.0.0.1` and a
/// hostname whose A record is `10.0.0.5` are all loopback or private, and none
/// of them contains the text "127.0.0.1". A blocklist of names is not a
/// defence; asking the resolver is.
///
/// Every address a host resolves to must be public. A name with one public and
/// one private answer is refused — that shape is how the check gets walked
/// past, not a legitimate configuration a crawler needs.
///
/// **This is not proof against DNS rebinding.** The name is resolved here and
/// again by the HTTP client when it connects, and a name can answer
/// differently between the two. Closing that needs the socket, which belongs
/// to the HTTP client. The redirect check in the agent's fetcher is the
/// stronger half of this defence; this one stops a crawl being pointed at a
/// private address in the first place.
pub fn is_routable_public(host: &str) -> bool {
    // A literal address answers without a resolver, and must, because a
    // resolver is not obliged to accept one.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(&ip);
    }
    if let Some(ip) = parse_relaxed_ipv4(host) {
        return is_public_v4(&ip);
    }
    // Bracketed IPv6 as it appears in a URL authority.
    let unbracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    if let Some(inner) = unbracketed {
        return match inner.parse::<IpAddr>() {
            Ok(ip) => is_public_ip(&ip),
            Err(_) => false,
        };
    }
    // Port 80 is arbitrary: `to_socket_addrs` needs one and only the address
    // is being judged.
    match (host, 80u16).to_socket_addrs() {
        // An empty answer is not an endorsement.
        Ok(addrs) => {
            let mut saw_one = false;
            for addr in addrs {
                saw_one = true;
                if !is_public_ip(&addr.ip()) {
                    return false;
                }
            }
            saw_one
        }
        // A name that will not resolve cannot be fetched anyway, and refusing
        // is the safe direction for a check whose job is to refuse.
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address parses")
    }

    #[test]
    fn the_addresses_a_crawler_must_not_reach() {
        for private in [
            "127.0.0.1",                    // loopback
            "127.9.9.9",                    // all of 127/8
            "0.0.0.0",                      // routes to local on many stacks
            "10.1.2.3",                     // RFC1918
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",              // cloud instance metadata
            "100.64.0.1",                   // carrier NAT — and the tailnet range
            "255.255.255.255",
            "::1",                          // IPv6 loopback
            "::",
            "fd00::1",                      // unique local
            "fe80::1",                      // link local
            "::ffff:127.0.0.1",             // IPv4 loopback wearing IPv6
            "::ffff:169.254.169.254",
        ] {
            assert!(
                !is_public_ip(&ip(private)),
                "{private} must not be reachable by the crawler"
            );
        }
    }

    #[test]
    fn ordinary_public_addresses_still_work() {
        // Refusing everything would pass the test above and break the crawler.
        for public in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "172.32.0.1", "2606:4700::1111"] {
            assert!(is_public_ip(&ip(public)), "{public} is a public address");
        }
    }

    #[test]
    fn the_boundaries_of_each_private_range() {
        // One address either side, because an off-by-one here either opens a
        // hole or silently refuses a chunk of the internet.
        assert!(!is_public_ip(&ip("172.16.0.0")), "start of RFC1918 172.16/12");
        assert!(!is_public_ip(&ip("172.31.255.255")), "end of it");
        assert!(is_public_ip(&ip("172.15.255.255")), "just below");
        assert!(is_public_ip(&ip("172.32.0.0")), "just above");
        assert!(!is_public_ip(&ip("100.64.0.0")), "start of carrier NAT");
        assert!(!is_public_ip(&ip("100.127.255.255")), "end of it");
        assert!(is_public_ip(&ip("100.63.255.255")), "just below");
        assert!(is_public_ip(&ip("100.128.0.0")), "just above");
    }

    #[test]
    fn a_literal_address_is_judged_without_a_resolver() {
        assert!(!is_routable_public("127.0.0.1"));
        assert!(!is_routable_public("169.254.169.254"));
        assert!(!is_routable_public("[::1]"));
        assert!(!is_routable_public("[::ffff:127.0.0.1]"));
        assert!(is_routable_public("1.1.1.1"));
    }

    /// Loopback written so it does not look like loopback.
    ///
    /// Reported by review. None of these parse as an address with the strict
    /// parser, and Forge's own URL parser does not normalise them — so the
    /// host arrived at the guard spelled exactly as written, was judged "not
    /// obviously local", and the HTTP client normalised it afterwards and
    /// connected to 127.0.0.1. Every one of these is a working way to reach
    /// the local machine from a browser.
    #[test]
    fn the_obfuscated_spellings_of_loopback_are_recognised() {
        for spelling in [
            "2130706433",       // decimal
            "127.1",            // short form, last part fills two octets
            "0x7f.1",           // hex first octet
            "017700000001",     // octal
            "0x7f000001",       // hex, whole address
            "127.0x0.0x0.1",    // mixed
        ] {
            assert!(
                is_obviously_local(spelling),
                "{spelling} reaches loopback and was not recognised"
            );
            assert!(!is_routable_public(spelling), "{spelling}");
        }
    }

    #[test]
    fn an_ordinary_hostname_is_not_mistaken_for_an_address() {
        // The relaxed parser must not swallow names. A false positive here
        // refuses a legitimate site.
        for name in [
            "example.com",
            "1.example.com",
            "0x.example",
            "999",              // out of range for one part? no — but not a name
            "example.",         // trailing dot
            "12.34.56.78.90",   // five parts
            "1.2.3.4.5",
        ] {
            let relaxed = super::parse_relaxed_ipv4(name);
            if let Some(ip) = relaxed {
                // If it did parse, it must be a real address, not a name.
                assert!(
                    name.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'x' || c.is_ascii_hexdigit()),
                    "{name} was parsed as {ip}"
                );
            }
        }
        // Public numeric addresses still pass.
        assert!(!is_obviously_local("8.8.8.8"));
        assert!(!is_obviously_local("134744072")); // 8.8.8.8 in decimal
        assert!(is_routable_public("134744072"));
    }

    #[test]
    fn a_name_that_cannot_resolve_is_refused_not_allowed() {
        // The safe direction for a check whose only job is to refuse. The
        // name is reserved by RFC 2606 and cannot resolve.
        assert!(!is_routable_public("nothing.invalid"));
        assert!(!is_routable_public(""));
    }
}
