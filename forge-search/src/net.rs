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
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()            // 127/8
        || ip.is_private()        // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()     // 169.254/16 — cloud instance metadata
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()    // 0.0.0.0, which many stacks route to local
        || a == 0                 // 0/8 "this network"
        || (a == 100 && (64..128).contains(&b)) // 100.64/10 carrier NAT
        // 192.0.0/24 IETF protocol assignments. The third octet matters: the
        // test was `a == 192 && b == 0`, which is 192.0.0/16 — refusing about
        // 253 globally routable /24s, 192.0.1/24 through 192.0.255/24, as if
        // they were reserved. 192.0.2/24 is documentation and is caught by
        // `is_documentation` above.
        || (a == 192 && b == 0 && c == 0)
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
    let bare = normalise_host(host);
    let bare = bare.as_str();

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
    let lower = bare.to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "broadcasthost"
}

/// One spelling of a host, before anything tries to judge it.
///
/// Strips the brackets an IPv6 literal wears in a URL authority, and the
/// root dot a fully-qualified name may end with. `127.0.0.1.` is the same
/// machine as `127.0.0.1` to every resolver, and bailing on the dot rather
/// than removing it left the canonical form walking past the check — which
/// is the sort of thing a guard is for.
fn normalise_host(host: &str) -> String {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.trim_end_matches('.').to_string()
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
    if host.is_empty() {
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
    let host = &normalise_host(host);
    // A literal address answers without a resolver, and must, because a
    // resolver is not obliged to accept one.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(&ip);
    }
    if let Some(ip) = parse_relaxed_ipv4(host) {
        return is_public_v4(&ip);
    }
    // Port 80 is arbitrary: `to_socket_addrs` needs one and only the address
    // is being judged.
    match (host.as_str(), 80u16).to_socket_addrs() {
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

    /// The fully-qualified form of a name is the same machine.
    ///
    /// `127.0.0.1.` with the root dot is what a resolver is handed for an
    /// absolute name, and the relaxed parser bailed on the dot rather than
    /// removing it — so the canonical spelling of loopback walked past the
    /// guard. Reported by review.
    #[test]
    fn a_trailing_root_dot_does_not_hide_an_address() {
        for spelling in ["127.0.0.1.", "169.254.169.254.", "2130706433.", "localhost.", "10.0.0.1."] {
            assert!(is_obviously_local(spelling), "{spelling} walked past the guard");
            assert!(!is_routable_public(spelling), "{spelling}");
        }
        // And a public name with a root dot is still public.
        assert!(!is_obviously_local("8.8.8.8."));
        assert!(is_routable_public("8.8.8.8."));
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
#[cfg(test)]
mod reserved_range_tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn public(a: u8, b: u8, c: u8, d: u8) -> bool {
        is_public_ip(&std::net::IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
    }

    /// 192.0.0/24 is reserved; the rest of 192.0/16 is not.
    ///
    /// The guard tested only the first two octets, so every address from
    /// 192.0.1.0 to 192.0.255.255 was refused as reserved. That is roughly 253
    /// routable /24s a crawl could not reach, for a range whose own comment
    /// said /24.
    #[test]
    fn only_the_first_slash_24_of_192_0_is_reserved() {
        assert!(!public(192, 0, 0, 1), "192.0.0/24 is IETF protocol assignments");
        assert!(!public(192, 0, 0, 170), "192.0.0/24, DNS64 discovery");
        // 192.0.2/24 is TEST-NET-1, caught as documentation.
        assert!(!public(192, 0, 2, 1), "192.0.2/24 is documentation");

        assert!(public(192, 0, 1, 1), "192.0.1/24 is globally routable");
        assert!(public(192, 0, 3, 1), "192.0.3/24 is globally routable");
        assert!(public(192, 0, 255, 1), "192.0.255/24 is globally routable");
    }

    /// The neighbouring ranges the same guard covers, so narrowing one did not
    /// widen another.
    #[test]
    fn the_other_reserved_ranges_still_refuse() {
        assert!(!public(127, 0, 0, 1));
        assert!(!public(10, 0, 0, 1));
        assert!(!public(172, 16, 0, 1));
        assert!(!public(192, 168, 0, 1));
        assert!(!public(169, 254, 169, 254), "cloud instance metadata");
        assert!(!public(100, 64, 0, 1), "carrier NAT");
        assert!(!public(198, 18, 0, 1), "benchmarking");
        assert!(!public(0, 0, 0, 0));
        assert!(!public(255, 255, 255, 255));
        assert!(!public(240, 0, 0, 1));

        assert!(public(8, 8, 8, 8));
        assert!(public(1, 1, 1, 1));
        assert!(public(192, 167, 0, 1), "one below the private range");
        assert!(public(192, 169, 0, 1), "one above the private range");
    }
}

