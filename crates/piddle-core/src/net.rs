//! Network connections of a single process, looked up on demand.
//!
//! Linux has no cheap per-process bandwidth counter, but a process's sockets
//! can be matched to the kernel's connection tables: socket inodes come from
//! `/proc/<pid>/fd`, and the tables are read from `/proc/<pid>/net/*` so they
//! reflect the process's own network namespace (containers included).
//! Reading another user's `fd` directory needs root, so this returns None then.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub proto: Proto,
    pub local: SocketAddr,
    /// None for listening / unconnected sockets.
    pub remote: Option<SocketAddr>,
    pub state: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sockets {
    /// TCP and UDP sockets, listeners first, then by local port.
    pub connections: Vec<Connection>,
    pub unix: usize,
    /// Socket file descriptors whose inode wasn't found in any table (e.g. netlink).
    pub other: usize,
}

impl Sockets {
    pub fn listening(&self) -> usize {
        self.connections.iter().filter(|c| c.state == "LISTEN").count()
    }
}

/// All sockets held by `pid`, or None if its file descriptors aren't readable.
pub fn sockets_of(pid: i32) -> Option<Sockets> {
    let mut inodes: Vec<u64> = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path())
            && let Some(inode) =
                target.to_str().and_then(|t| t.strip_prefix("socket:[")).and_then(|t| t.strip_suffix(']'))
            && let Ok(inode) = inode.parse()
        {
            inodes.push(inode);
        }
    }
    let mut out = Sockets::default();
    if inodes.is_empty() {
        return Some(out);
    }

    let mut table: HashMap<u64, Connection> = HashMap::new();
    for (file, proto) in [("tcp", Proto::Tcp), ("tcp6", Proto::Tcp), ("udp", Proto::Udp), ("udp6", Proto::Udp)] {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{file}")) {
            for line in text.lines().skip(1) {
                if let Some((inode, conn)) = parse_inet_line(line, proto) {
                    table.insert(inode, conn);
                }
            }
        }
    }
    let unix: std::collections::HashSet<u64> = std::fs::read_to_string(format!("/proc/{pid}/net/unix"))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(6)?.parse().ok())
        .collect();

    for inode in inodes {
        if let Some(c) = table.remove(&inode) {
            out.connections.push(c);
        } else if unix.contains(&inode) {
            out.unix += 1;
        } else {
            out.other += 1;
        }
    }
    out.connections.sort_by_key(|c| (c.state != "LISTEN", c.proto != Proto::Tcp, c.local.port()));
    Some(out)
}

/// One line of /proc/net/{tcp,udp}[6]:
/// `sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode ...`
fn parse_inet_line(line: &str, proto: Proto) -> Option<(u64, Connection)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    let local = parse_addr(f.get(1)?)?;
    let remote = parse_addr(f.get(2)?)?;
    let st = u8::from_str_radix(f.get(3)?, 16).ok()?;
    let inode: u64 = f.get(9)?.parse().ok()?;
    let state = match proto {
        Proto::Tcp => tcp_state(st),
        // UDP has no states; 07 (TCP_CLOSE) means unconnected, 01 means connect()ed.
        Proto::Udp => {
            if st == 1 {
                "CONNECTED"
            } else {
                "OPEN"
            }
        }
    };
    let remote = (!remote.ip().is_unspecified() || remote.port() != 0).then_some(remote);
    Some((inode, Connection { proto, local, remote, state }))
}

/// `0100007F:0050` (IPv4) or 32 hex digits + port (IPv6). The kernel prints
/// each 32-bit word of the address in host (little-endian) byte order.
fn parse_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(ip, 16).ok()?.to_le_bytes())),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let word = u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok()?;
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            // Show IPv4-mapped addresses (::ffff:a.b.c.d) as plain IPv4.
            v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn tcp_state(st: u8) -> &'static str {
    match st {
        0x01 => "ESTABLISHED",
        0x02 => "SYN_SENT",
        0x03 => "SYN_RECV",
        0x04 => "FIN_WAIT1",
        0x05 => "FIN_WAIT2",
        0x06 => "TIME_WAIT",
        0x07 => "CLOSE",
        0x08 => "CLOSE_WAIT",
        0x09 => "LAST_ACK",
        0x0A => "LISTEN",
        0x0B => "CLOSING",
        _ => "UNKNOWN",
    }
}

/// One-line summary, e.g. "3 TCP (1 listening), 1 UDP, 12 unix".
pub fn summary(s: &Sockets) -> String {
    let tcp = s.connections.iter().filter(|c| c.proto == Proto::Tcp).count();
    let udp = s.connections.len() - tcp;
    let mut parts = Vec::new();
    if tcp > 0 {
        let l = s.listening();
        parts.push(if l > 0 { format!("{tcp} TCP ({l} listening)") } else { format!("{tcp} TCP") });
    }
    if udp > 0 {
        parts.push(format!("{udp} UDP"));
    }
    if s.unix > 0 {
        parts.push(format!("{} unix", s.unix));
    }
    if s.other > 0 {
        parts.push(format!("{} other", s.other));
    }
    if parts.is_empty() { "no sockets".into() } else { parts.join(", ") }
}

/// "tcp  127.0.0.1:8080 → 10.0.0.2:51234  ESTABLISHED" style line.
pub fn describe(c: &Connection) -> String {
    match c.remote {
        Some(r) => format!("{} {} → {} {}", c.proto.name(), c.local, r, c.state),
        None => format!("{} {} {}", c.proto.name(), c.local, c.state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_and_ipv6_lines() {
        let v4 = "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let (inode, c) = parse_inet_line(v4, Proto::Tcp).unwrap();
        assert_eq!(inode, 12345);
        assert_eq!(c.local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(c.remote, None);
        assert_eq!(c.state, "LISTEN");

        let v6 = "   1: 00000000000000000000000001000000:0016 0000000000000000FFFF00000200000A:C350 01 00000000:00000000 00:00000000 00000000  1000        0 777 1";
        let (inode, c) = parse_inet_line(v6, Proto::Tcp).unwrap();
        assert_eq!(inode, 777);
        assert_eq!(c.local, "[::1]:22".parse().unwrap());
        assert_eq!(c.remote, Some("10.0.0.2:50000".parse().unwrap()));
        assert_eq!(c.state, "ESTABLISHED");
    }

    #[test]
    fn finds_own_listening_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = sockets_of(std::process::id() as i32).unwrap();
        assert!(s.connections.iter().any(|c| c.local.port() == port && c.state == "LISTEN"), "{s:?}");
        assert!(summary(&s).contains("listening"));
    }
}
