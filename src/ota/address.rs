//! Reading a board's own IPv4 address out of what it prints when it boots.
//!
//! The address an over-the-air update is pushed to is the one fact of the
//! `[ota]` section a project cannot record for itself: DHCP hands it out,
//! and it moves. Until now it was typed by hand into
//! [`crate::app::Overlay::OtaAddress`], with nothing to check it against
//! --- a wrong digit surfaced minutes later as a timeout partway through
//! an upload.
//!
//! Zephyr already says it. Two subsystems print the address on the console
//! at `LOG_LEVEL_INF`, and between them they cover both ways a board can
//! have one:
//!
//! * `net_dhcpv4: Received: <addr>` --- the lease, from
//!   `subsys/net/lib/dhcpv4/dhcpv4.c`;
//! * `net_config: IPv4 address: <addr>` --- from
//!   `subsys/net/lib/config/init.c`, which prints it for a **static**
//!   address as well as a leased one.
//!
//! So this module is the counterpart of [`crate::firmware_id::version`]:
//! a pure `&str -> Option<String>`, fed by
//! [`crate::app::AddressCapture`]'s live console the way the boot-banner
//! capture feeds the version reader. Nothing here touches the network ---
//! the board is the one making the claim, and it is talking over its own
//! UART.
//!
//! Neither line exists unless the project's Kconfig turns those log
//! levels on, which is what [`super::prepare::Step::AddressLog`]'s block
//! is for.

/// The anchors, most specific first. Each is the literal text preceding
/// the address on its line; the log's own `[timestamp] <inf> ` prefix is
/// deliberately not part of the match, so a project that changed
/// `LOG_MODE`, dropped timestamps or colours the level still parses.
const ANCHORS: [&str; 2] = ["net_dhcpv4: Received: ", "net_config: IPv4 address: "];

/// The board's IPv4 address as its own console reported it, or `None` when
/// nothing in `text` says.
///
/// The **last** match wins: a board prints its address again on every
/// renewal and after a reconnect, and the newest line is the one still
/// true. A run of both anchors in one boot names the same address anyway.
///
/// `0.0.0.0` is rejected --- `net_config` prints it for an interface that
/// has not got one yet, and recording it would leave `[ota] address`
/// looking answered while pointing nowhere.
pub fn from_console(text: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        for anchor in ANCHORS {
            let Some((_, tail)) = line.split_once(anchor) else {
                continue;
            };
            if let Some(address) = leading_ipv4(tail) {
                found = Some(address);
            }
        }
    }
    found
}

/// The dotted quad `text` starts with, if it does. Reads the address off
/// the front rather than searching, because the anchor already located
/// it: what follows on a `net_config` line is a second field
/// (`Lease time`, `Subnet`), not another candidate.
fn leading_ipv4(text: &str) -> Option<String> {
    let end = text
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(text.len());
    let candidate = &text[..end];
    let octets: Vec<&str> = candidate.split('.').collect();
    if octets.len() != 4 {
        return None;
    }
    // Each octet must be a number that fits, and be spelled as one: an
    // empty field (`1..2.3`) parses to nothing and a four-digit one is not
    // an address.
    if !octets
        .iter()
        .all(|octet| !octet.is_empty() && octet.len() <= 3 && octet.parse::<u8>().is_ok())
    {
        return None;
    }
    if candidate == "0.0.0.0" {
        return None;
    }
    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DHCPv4 line, in the shape Zephyr's log frontend prints it.
    #[test]
    fn the_dhcp_lease_line_names_the_address() {
        let text = "\
*** Booting Zephyr OS build v4.0.0 ***
[00:00:02.145,000] <inf> net_dhcpv4: Received: 192.168.1.42
[00:00:02.146,000] <inf> net_dhcpv4: Lease time: 43200 seconds
";
        assert_eq!(from_console(text), Some("192.168.1.42".to_string()));
    }

    /// `net_config` prints the same fact for a *static* address, which is
    /// the whole reason both anchors are read: a project that never runs a
    /// DHCP client is still captured.
    #[test]
    fn the_static_address_line_is_read_too() {
        let text = "[00:00:00.310,000] <inf> net_config: IPv4 address: 10.0.0.7\n";
        assert_eq!(from_console(text), Some("10.0.0.7".to_string()));
    }

    /// A board prints its address again on every renewal; the newest line
    /// is the one still true.
    #[test]
    fn the_last_address_wins() {
        let text = "\
[00:00:02.145,000] <inf> net_dhcpv4: Received: 192.168.1.42
[00:20:11.900,000] <inf> net_dhcpv4: Received: 192.168.1.57
";
        assert_eq!(from_console(text), Some("192.168.1.57".to_string()));
    }

    /// The address is read off the anchor, so the field *after* it on the
    /// same line is not a second candidate.
    #[test]
    fn a_trailing_field_is_not_mistaken_for_the_address() {
        let text = "<inf> net_config: IPv4 address: 10.0.0.7 (netmask 255.255.255.0)\n";
        assert_eq!(from_console(text), Some("10.0.0.7".to_string()));
    }

    /// `net_config` prints `0.0.0.0` for an interface that has not got an
    /// address yet. Recording it would leave the key looking answered
    /// while pointing nowhere.
    #[test]
    fn the_unassigned_address_is_not_an_answer() {
        let text = "<inf> net_config: IPv4 address: 0.0.0.0\n";
        assert_eq!(from_console(text), None);
    }

    /// Boot output with the log levels off says nothing --- which is the
    /// state every unprepared project is in, and what the capture reports
    /// as a miss rather than a wrong answer.
    #[test]
    fn output_without_the_lines_answers_nothing() {
        let text = "\
*** Booting Zephyr OS build v4.0.0 ***
[00:00:01.002,000] <inf> app: connecting to home-wifi
uart:~$
";
        assert_eq!(from_console(text), None);
    }

    /// Malformed quads are not addresses: too few fields, an empty field,
    /// an octet past 255, and one too long to be an octet at all.
    #[test]
    fn a_malformed_quad_is_refused() {
        for tail in [
            "net_dhcpv4: Received: 192.168.1",
            "net_dhcpv4: Received: 192..1.42",
            "net_dhcpv4: Received: 192.168.1.256",
            "net_dhcpv4: Received: 1920.168.1.4",
            "net_dhcpv4: Received: not-an-address",
        ] {
            assert_eq!(from_console(tail), None, "{tail}");
        }
    }
}
