//! Phase B `unrestricted` networking (design §9): a virtio-net NIC backed by a pinned userspace
//! network stack, with an egress filter in the helper between the two.
//!
//! ```text
//!  guest eth0 ── libkrun virtio-net ══ socketpair ══ helper egress filter ══ socketpair ══ netstack ── host sockets
//!                (krun_add_net_unixstream, fd)        (this module)                    (passt on Linux,
//!                                                                                       gvproxy on macOS)
//! ```
//!
//! The netstack turns guest TCP/UDP flows into ordinary sockets of the (unprivileged) host user.
//! On its own it lets the guest reach whatever that user can reach, including services on the
//! host itself. The spike (design §9.1) showed:
//!
//! - **gvproxy** maps its "host" address `192.168.127.254` to the host's `127.0.0.1` (NAT), so a
//!   plain `connect()` from the guest reaches host loopback services. Its gateway also serves a
//!   port-forwarding HTTP API on `192.168.127.1:80` that can open listeners on the host.
//! - **passt** (`--no-map-gw`) drops guest frames addressed to loopback or `0.0.0.0`, but a
//!   connection to one of the host's own addresses reaches every service bound to `0.0.0.0`.
//!
//! So every guest → netstack frame passes [`EgressPolicy::check`] first. Only IPv4 and ARP get
//! through (no IPv6, no VLAN tags). IPv4 to the virtual gateway is limited to DNS (53) and DHCP
//! (67), broadcast to DHCP. Everything else is refused when it goes to a loopback, private,
//! link-local (cloud metadata), CGNAT, reserved or multicast range ([`ALWAYS_DENIED`], the same
//! set the Firecracker nftables policy blocks), to one of the host's own addresses or on-link
//! subnets ([`host_prefixes`]), or to an SMTP port ([`SMTP_PORTS`]). Frames from the netstack to
//! the guest are only length-checked: the netstacks open no inbound ports (passt `-t none -u
//! none`; gvproxy has no forwards and its API is unreachable through the filter).
//!
//! The guest gets the same addressing on both platforms: `192.168.127.2/24`, gateway and
//! resolver `192.168.127.1`, by DHCP from the netstack (libkrun's `init.krun` client,
//! `NET_FLAG_DHCP_CLIENT`), MTU 1500, no offloads.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The guest's address (DHCP lease from the netstack).
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
/// Virtual gateway and DNS resolver.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
/// Prefix length of the virtual subnet.
pub const SUBNET_PREFIX: u8 = 24;
/// The guest NIC's MAC (gvproxy's static DHCP lease for [`GUEST_IP`]).
pub const GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
/// Link MTU. No virtio-net offloads are negotiated, so frames stay at most MTU + 14 bytes.
pub const MTU: u16 = 1500;
/// Outbound SMTP is refused (spam from the host's IP gets it blocklisted), like Firecracker.
pub const SMTP_PORTS: [u16; 3] = [25, 465, 587];
/// Largest frame accepted on either side (an Ethernet frame of a 64 KiB IP packet).
pub const MAX_FRAME: usize = 65_535 + 14;

/// IPv4 ranges the guest may never reach (RFC 6890 special-purpose registry, private and
/// shared address space, link-local incl. cloud metadata). The virtual subnet itself is in
/// `192.168.0.0/16`; the gateway's DNS/DHCP are explicit exceptions.
pub const ALWAYS_DENIED: [&str; 14] = [
    "0.0.0.0/8",       // "this network"
    "10.0.0.0/8",      // private
    "100.64.0.0/10",   // CGNAT (tailnets)
    "127.0.0.0/8",     // loopback
    "169.254.0.0/16",  // link-local, cloud metadata
    "172.16.0.0/12",   // private
    "192.0.0.0/24",    // IETF protocol assignments
    "192.0.2.0/24",    // documentation
    "192.168.0.0/16",  // private (incl. the virtual subnet)
    "198.18.0.0/15",   // benchmarking
    "198.51.100.0/24", // documentation
    "203.0.113.0/24",  // documentation
    "224.0.0.0/4",     // multicast
    "240.0.0.0/4",     // reserved, incl. 255.255.255.255
];

/// Most host prefixes passed to the helper (validation bound).
pub const MAX_DENY_PREFIXES: usize = 256;

// ---------------------------------------------------------------------------------------------
// Netstack kinds

/// Which userspace network stack backs the guest NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetstackKind {
    /// [passt](https://passt.top) (Linux), qemu stream framing on an inherited fd.
    Passt,
    /// [gvproxy](https://github.com/containers/gvisor-tap-vsock) (macOS), stdio framing.
    Gvproxy,
}

impl NetstackKind {
    /// The kind used on this host (`None` where libkrun is unsupported).
    #[must_use]
    pub fn for_host() -> Option<Self> {
        match std::env::consts::OS {
            "linux" => Some(Self::Passt),
            "macos" => Some(Self::Gvproxy),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passt => "passt",
            Self::Gvproxy => "gvproxy",
        }
    }

    /// Length-prefix format on the helper ↔ netstack socket.
    #[must_use]
    pub fn framing(self) -> Framing {
        match self {
            Self::Passt => Framing::Be32,
            Self::Gvproxy => Framing::Le16,
        }
    }

    /// Arguments (after argv\[0\]). The netstack's end of the socketpair is its stdin (passt:
    /// `--fd 0`; gvproxy: stdin + stdout).
    ///
    /// - passt: foreground (`-f`), quiet, IPv4 only, the fixed guest addressing, `--no-map-gw`
    ///   (the gateway is *not* the host's loopback), DNS to the gateway forwarded to the host's
    ///   resolver, no inbound port forwarding (`-t none -u none`), no host resolver or search
    ///   list leaked over DHCP. `--fd` implies one-off: passt exits when the helper's end
    ///   closes.
    /// - gvproxy: stdio transport (it exits on EOF), the default `192.168.127.0/24` network,
    ///   no SSH forward (`-ssh-port -1`, which would listen on the host), no `-listen` /
    ///   `-services` API endpoints, no EC2 metadata access.
    #[must_use]
    pub fn args(self) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        match self {
            Self::Passt => {
                let mut a = s(&["--foreground", "--quiet", "--fd", "0", "--ipv4-only"]);
                a.extend([
                    "--mtu".into(),
                    MTU.to_string(),
                    "--address".into(),
                    GUEST_IP.to_string(),
                    "--netmask".into(),
                    SUBNET_PREFIX.to_string(),
                    "--gateway".into(),
                    GATEWAY_IP.to_string(),
                    "--dns-forward".into(),
                    GATEWAY_IP.to_string(),
                ]);
                a.extend(s(&[
                    "--no-map-gw",
                    "--no-dhcp-dns",
                    "--no-dhcp-search",
                    "-t",
                    "none",
                    "-u",
                    "none",
                ]));
                a
            }
            Self::Gvproxy => {
                let mut a = s(&["-listen-stdio", "accept", "-ssh-port", "-1"]);
                a.extend(["-mtu".into(), MTU.to_string()]);
                a
            }
        }
    }
}

impl fmt::Display for NetstackKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Framing

/// Length-prefixed Ethernet frames on a stream socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// 4-byte big-endian length (qemu stream / passt; libkrun's unixstream backend).
    Be32,
    /// 2-byte little-endian length (gvproxy `-listen-stdio`, "hyperkit without handshake").
    Le16,
}

impl Framing {
    /// Read one frame into `buf`. `Ok(false)` on a clean EOF before a header. Frames longer
    /// than [`MAX_FRAME`] are an error (the stream cannot be trusted any more).
    pub fn read_frame<R: BufRead>(self, r: &mut R, buf: &mut Vec<u8>) -> io::Result<bool> {
        let len = match self {
            Self::Be32 => {
                let mut h = [0u8; 4];
                if !read_header(r, &mut h)? {
                    return Ok(false);
                }
                u32::from_be_bytes(h) as usize
            }
            Self::Le16 => {
                let mut h = [0u8; 2];
                if !read_header(r, &mut h)? {
                    return Ok(false);
                }
                usize::from(u16::from_le_bytes(h))
            }
        };
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame of {len} bytes exceeds {MAX_FRAME}"),
            ));
        }
        buf.resize(len, 0);
        r.read_exact(buf)?;
        Ok(true)
    }

    /// Write one frame (header and payload in a single `write_all`).
    pub fn write_frame<W: Write>(self, w: &mut W, frame: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(frame.len() + 4);
        match self {
            Self::Be32 => {
                let n = u32::try_from(frame.len()).map_err(|_| too_big(frame.len()))?;
                out.extend_from_slice(&n.to_be_bytes());
            }
            Self::Le16 => {
                let n = u16::try_from(frame.len()).map_err(|_| too_big(frame.len()))?;
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        out.extend_from_slice(frame);
        w.write_all(&out)
    }
}

fn too_big(n: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("frame of {n} bytes does not fit"),
    )
}

/// Fill `h`; `Ok(false)` if the stream ended before its first byte.
fn read_header<R: Read>(r: &mut R, h: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < h.len() {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// IPv4 prefixes

/// An IPv4 prefix, `a.b.c.d/n` on the wire. The address is stored masked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    /// `addr/prefix` with the host bits cleared; `None` if `prefix > 32`.
    #[must_use]
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Option<Self> {
        (prefix <= 32).then(|| Self {
            addr: Ipv4Addr::from(u32::from(addr) & mask(prefix)),
            prefix,
        })
    }

    #[must_use]
    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }

    #[must_use]
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    #[must_use]
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.addr)
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (a, p) = s
            .split_once('/')
            .ok_or_else(|| format!("`{s}` is not a.b.c.d/n"))?;
        let addr: Ipv4Addr = a.parse().map_err(|e| format!("`{s}`: {e}"))?;
        let prefix: u8 = p.parse().map_err(|e| format!("`{s}`: {e}"))?;
        let net = Self::new(addr, prefix).ok_or_else(|| format!("`{s}`: prefix > 32"))?;
        if net.addr != addr {
            return Err(format!("`{s}` has host bits set (use {net})"));
        }
        Ok(net)
    }
}

impl Serialize for Ipv4Net {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ipv4Net {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// [`ALWAYS_DENIED`] parsed.
#[must_use]
pub fn always_denied() -> Vec<Ipv4Net> {
    ALWAYS_DENIED
        .iter()
        .map(|s| s.parse().expect("valid built-in prefix"))
        .collect()
}

/// The host's own IPv4 addresses (as `/32`) and their on-link subnets, from `getifaddrs`.
/// A guest connection to one of them would reach host services bound to `0.0.0.0` (and, on a
/// server with a public uplink, its gateway and neighbours). Loopback interfaces are skipped
/// (`127.0.0.0/8` is always denied). Taken at spawn time: an address the host acquires later
/// (DHCP renewal, a VPN coming up) is not covered until the next sandbox.
pub fn host_prefixes() -> io::Result<Vec<Ipv4Net>> {
    use nix::net::if_::InterfaceFlags;
    let mut out = Vec::new();
    for ifa in nix::ifaddrs::getifaddrs().map_err(io::Error::from)? {
        if ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK) {
            continue;
        }
        let Some(addr) = ifa.address.as_ref().and_then(|a| a.as_sockaddr_in()) else {
            continue;
        };
        let ip = addr.ip();
        out.push(Ipv4Net::new(ip, 32).expect("/32"));
        let prefix = ifa
            .netmask
            .as_ref()
            .and_then(|m| m.as_sockaddr_in())
            .map_or(32, |m| u32::from(m.ip()).leading_ones());
        // A /0 "subnet" (odd point-to-point setups) would deny everything: keep the /32 only.
        if (8..32).contains(&prefix) {
            out.push(Ipv4Net::new(ip, prefix as u8).expect("prefix ≤ 32"));
        }
        if let Some(peer) = ifa.destination.as_ref().and_then(|a| a.as_sockaddr_in()) {
            out.push(Ipv4Net::new(peer.ip(), 32).expect("/32"));
        }
    }
    Ok(minimize(out))
}

/// Sorted, deduplicated, without prefixes already inside [`ALWAYS_DENIED`] or another entry
/// (a host with many private interfaces, e.g. a Firecracker tap pool, stays small).
fn minimize(mut v: Vec<Ipv4Net>) -> Vec<Ipv4Net> {
    let builtin = always_denied();
    let covers = |outer: &Ipv4Net, inner: &Ipv4Net| {
        outer.prefix <= inner.prefix && outer.contains(inner.addr)
    };
    v.sort_unstable();
    v.dedup();
    let all = v.clone();
    v.retain(|n| {
        !builtin.iter().any(|b| covers(b, n)) && !all.iter().any(|o| o != n && covers(o, n))
    });
    v
}

// ---------------------------------------------------------------------------------------------
// Egress policy

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
const DNS_PORT: u16 = 53;
const DHCP_SERVER_PORT: u16 = 67;

/// Why a guest frame was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// Too short or a malformed IPv4 header.
    Malformed,
    /// Not IPv4 or ARP (IPv6, VLAN-tagged, …).
    EtherType(u16),
    /// To the virtual gateway, but not DNS/DHCP.
    GatewayService { proto: u8, port: Option<u16> },
    /// Broadcast that is not a DHCP request.
    Broadcast,
    /// The destination is in a denied prefix.
    Denied { dst: Ipv4Addr, net: Ipv4Net },
    /// TCP to an SMTP port.
    Smtp { dst: Ipv4Addr, port: u16 },
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed frame"),
            Self::EtherType(t) => write!(f, "ethertype {t:#06x} (only IPv4 and ARP)"),
            Self::GatewayService { proto, port } => match port {
                Some(p) => write!(
                    f,
                    "gateway {GATEWAY_IP} proto {proto} port {p} (only DNS/DHCP)"
                ),
                None => write!(f, "gateway {GATEWAY_IP} proto {proto} (only DNS/DHCP)"),
            },
            Self::Broadcast => f.write_str("broadcast other than DHCP"),
            Self::Denied { dst, net } => write!(f, "{dst} is in denied {net}"),
            Self::Smtp { dst, port } => write!(f, "SMTP to {dst}:{port}"),
        }
    }
}

/// The guest → netstack filter. Pure and allocation-free per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPolicy {
    deny: Vec<Ipv4Net>,
}

impl EgressPolicy {
    /// [`ALWAYS_DENIED`] plus `extra` (the host's prefixes).
    #[must_use]
    pub fn new(extra: &[Ipv4Net]) -> Self {
        let mut deny = always_denied();
        deny.extend_from_slice(extra);
        deny.sort_unstable();
        deny.dedup();
        Self { deny }
    }

    #[must_use]
    pub fn denied(&self) -> &[Ipv4Net] {
        &self.deny
    }

    /// `Ok(())` to forward the frame, `Err(why)` to drop it.
    pub fn check(&self, frame: &[u8]) -> Result<(), Denial> {
        if frame.len() < ETH_HDR {
            return Err(Denial::Malformed);
        }
        match u16::from_be_bytes([frame[12], frame[13]]) {
            ETHERTYPE_ARP => Ok(()),
            ETHERTYPE_IPV4 => self.check_ipv4(&frame[ETH_HDR..]),
            other => Err(Denial::EtherType(other)),
        }
    }

    fn check_ipv4(&self, ip: &[u8]) -> Result<(), Denial> {
        if ip.len() < 20 || ip[0] >> 4 != 4 {
            return Err(Denial::Malformed);
        }
        let ihl = usize::from(ip[0] & 0x0f) * 4;
        let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
        if ihl < 20 || total < ihl || total > ip.len() {
            return Err(Denial::Malformed);
        }
        let proto = ip[9];
        let dst = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
        let frag_offset = u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff;
        // Destination port, only in the first (or only) fragment of TCP/UDP.
        let port = match proto {
            PROTO_TCP | PROTO_UDP if frag_offset == 0 && total >= ihl + 4 => {
                Some(u16::from_be_bytes([ip[ihl + 2], ip[ihl + 3]]))
            }
            _ => None,
        };

        if dst == GATEWAY_IP {
            return match (proto, port) {
                (PROTO_UDP | PROTO_TCP, Some(DNS_PORT)) | (PROTO_UDP, Some(DHCP_SERVER_PORT)) => {
                    Ok(())
                }
                _ => Err(Denial::GatewayService { proto, port }),
            };
        }
        if dst == Ipv4Addr::BROADCAST {
            return match (proto, port) {
                (PROTO_UDP, Some(DHCP_SERVER_PORT)) => Ok(()),
                _ => Err(Denial::Broadcast),
            };
        }
        if let Some(net) = self.deny.iter().find(|n| n.contains(dst)) {
            return Err(Denial::Denied { dst, net: *net });
        }
        if let (PROTO_TCP, Some(p)) = (proto, port)
            && SMTP_PORTS.contains(&p)
        {
            return Err(Denial::Smtp { dst, port: p });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Runtime (used by the helper)

/// Counters shared by the shuttle threads.
#[derive(Debug, Default)]
pub struct Stats {
    pub to_netstack: AtomicU64,
    pub to_guest: AtomicU64,
    pub dropped: AtomicU64,
}

/// Drops logged individually before going quiet (then every 1000th).
const LOGGED_DROPS: u64 = 32;

/// Copy frames guest → netstack through `policy`, until either side closes. Then shut both
/// sockets down so the other direction (and the peer processes) see EOF too.
pub fn shuttle_egress(
    guest: UnixStream,
    netstack: UnixStream,
    framing: Framing,
    policy: &EgressPolicy,
    stats: &Stats,
    log: &(dyn Fn(String) + Sync),
) -> io::Result<()> {
    let result = (|| {
        let mut r = BufReader::with_capacity(1 << 16, &guest);
        let mut w = &netstack;
        let mut buf = Vec::with_capacity(MAX_FRAME);
        while Framing::Be32.read_frame(&mut r, &mut buf)? {
            match policy.check(&buf) {
                Ok(()) => {
                    framing.write_frame(&mut w, &buf)?;
                    stats.to_netstack.fetch_add(1, Ordering::Relaxed);
                }
                Err(why) => {
                    let n = stats.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= LOGGED_DROPS || n.is_multiple_of(1000) {
                        log(format!("net: dropped guest frame #{n}: {why}"));
                    }
                }
            }
        }
        Ok(())
    })();
    let _ = guest.shutdown(Shutdown::Both);
    let _ = netstack.shutdown(Shutdown::Both);
    result
}

/// Copy frames netstack → guest (length-checked only), until either side closes.
pub fn shuttle_ingress(
    netstack: UnixStream,
    guest: UnixStream,
    framing: Framing,
    stats: &Stats,
) -> io::Result<()> {
    let result = (|| {
        let mut r = BufReader::with_capacity(1 << 16, &netstack);
        let mut w = &guest;
        let mut buf = Vec::with_capacity(MAX_FRAME);
        while framing.read_frame(&mut r, &mut buf)? {
            Framing::Be32.write_frame(&mut w, &buf)?;
            stats.to_guest.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })();
    let _ = guest.shutdown(Shutdown::Both);
    let _ = netstack.shutdown(Shutdown::Both);
    result
}

/// A started netstack: the guest end of the NIC socket (for `krun_add_net_unixstream`), the
/// process and the counters.
#[derive(Debug)]
pub struct Started {
    pub guest_fd: OwnedFd,
    pub child: Child,
    pub stats: Arc<Stats>,
}

/// Spawn `binary` (already verified by the caller; on Linux the `/proc/self/fd/<n>` of the
/// verified fd) as `kind`, with `argv0` (the pinned path: passt picks its mode from the
/// name) and an empty environment, its stdin (gvproxy: also stdout) on a fresh socketpair,
/// stderr inherited; start the two filter threads. The netstack exits when the helper's end
/// closes (EOF), i.e. when the helper process dies for any reason. It inherits the helper's
/// process group (torn down with it) and, on Linux, its cgroup leaf.
pub fn start(
    kind: NetstackKind,
    binary: &Path,
    argv0: &Path,
    policy: EgressPolicy,
    log: Arc<dyn Fn(String) + Send + Sync>,
) -> io::Result<Started> {
    use std::os::unix::process::CommandExt;
    // `UnixStream::pair` sets CLOEXEC on both ends: only the dup'd stdio reaches the child.
    let (guest_vm_end, guest_helper_end) = UnixStream::pair()?;
    let (ns_helper_end, ns_child_end) = UnixStream::pair()?;

    let mut cmd = Command::new(binary);
    cmd.arg0(argv0)
        .args(kind.args())
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::from(OwnedFd::from(ns_child_end.try_clone()?)));
    if kind == NetstackKind::Gvproxy {
        cmd.stdout(Stdio::from(OwnedFd::from(ns_child_end.try_clone()?)));
    }
    let child = cmd.spawn()?;
    drop(ns_child_end);
    drop(cmd);

    let stats = Arc::new(Stats::default());
    let framing = kind.framing();
    {
        let (g, n, s, l) = (
            guest_helper_end.try_clone()?,
            ns_helper_end.try_clone()?,
            Arc::clone(&stats),
            Arc::clone(&log),
        );
        std::thread::Builder::new()
            .name("net-egress".into())
            .spawn(move || {
                if let Err(e) = shuttle_egress(g, n, framing, &policy, &s, &*l) {
                    l(format!("net: guest → {kind} stopped: {e}"));
                }
            })?;
    }
    {
        let (s, l) = (Arc::clone(&stats), Arc::clone(&log));
        std::thread::Builder::new()
            .name("net-ingress".into())
            .spawn(move || {
                if let Err(e) = shuttle_ingress(ns_helper_end, guest_helper_end, framing, &s) {
                    l(format!("net: {kind} → guest stopped: {e}"));
                }
            })?;
    }
    Ok(Started {
        guest_fd: OwnedFd::from(guest_vm_end),
        child,
        stats,
    })
}

#[cfg(test)]
pub(crate) mod frames {
    //! Hand-built Ethernet/IPv4 frames for tests.
    use super::*;

    pub fn eth(ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xdd];
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    /// IPv4 + 8-byte L4 header stub (ports at the usual offsets).
    pub fn ipv4(dst: Ipv4Addr, proto: u8, dport: u16, frag: u16) -> Vec<u8> {
        let mut ip = vec![0x45, 0, 0, 0, 0, 1];
        ip.extend_from_slice(&frag.to_be_bytes());
        ip.extend_from_slice(&[64, proto, 0, 0]);
        ip.extend_from_slice(&GUEST_IP.octets());
        ip.extend_from_slice(&dst.octets());
        ip.extend_from_slice(&40000u16.to_be_bytes());
        ip.extend_from_slice(&dport.to_be_bytes());
        ip.extend_from_slice(&[0; 4]);
        let total = u16::try_from(ip.len()).unwrap();
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        eth(ETHERTYPE_IPV4, &ip)
    }

    pub fn tcp(dst: &str, port: u16) -> Vec<u8> {
        ipv4(dst.parse().unwrap(), PROTO_TCP, port, 0)
    }

    pub fn udp(dst: &str, port: u16) -> Vec<u8> {
        ipv4(dst.parse().unwrap(), PROTO_UDP, port, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::frames::*;
    use super::*;

    fn policy() -> EgressPolicy {
        EgressPolicy::new(&[
            "45.33.0.0/24".parse().unwrap(),
            "45.33.0.7/32".parse().unwrap(),
        ])
    }

    #[test]
    fn host_loopback_is_never_reachable() {
        let p = policy();
        for dst in ["127.0.0.1", "127.255.255.254", "0.0.0.0", "0.1.2.3"] {
            for f in [tcp(dst, 8080), udp(dst, 8080), tcp(dst, 53), udp(dst, 53)] {
                assert!(
                    matches!(p.check(&f), Err(Denial::Denied { .. })),
                    "{dst}: {:?}",
                    p.check(&f)
                );
            }
        }
        // gvproxy's NAT alias for the host's 127.0.0.1, any port.
        for port in [22, 53, 80, 8080] {
            assert!(matches!(
                p.check(&tcp("192.168.127.254", port)),
                Err(Denial::Denied { .. })
            ));
        }
    }

    #[test]
    fn gateway_only_serves_dns_and_dhcp() {
        let p = policy();
        assert_eq!(p.check(&udp("192.168.127.1", 53)), Ok(()));
        assert_eq!(p.check(&tcp("192.168.127.1", 53)), Ok(()));
        assert_eq!(p.check(&udp("192.168.127.1", 67)), Ok(()));
        // gvproxy's port-forwarding API, SSH, anything else.
        for port in [80, 22, 2222, 67, 443] {
            assert!(
                matches!(
                    p.check(&tcp("192.168.127.1", port)),
                    Err(Denial::GatewayService { .. })
                ),
                "tcp {port}"
            );
        }
        let icmp = ipv4(GATEWAY_IP, 1, 0, 0);
        assert!(matches!(
            p.check(&icmp),
            Err(Denial::GatewayService { port: None, .. })
        ));
        // A non-first fragment has no ports: never trusted as DNS.
        let frag = ipv4(GATEWAY_IP, PROTO_UDP, 53, 0x0010);
        assert!(matches!(
            p.check(&frag),
            Err(Denial::GatewayService { port: None, .. })
        ));
    }

    #[test]
    fn broadcast_only_for_dhcp() {
        let p = policy();
        assert_eq!(p.check(&udp("255.255.255.255", 67)), Ok(()));
        assert_eq!(p.check(&udp("255.255.255.255", 68)), Err(Denial::Broadcast));
        assert_eq!(p.check(&tcp("255.255.255.255", 67)), Err(Denial::Broadcast));
    }

    #[test]
    fn private_metadata_reserved_and_host_ranges_are_denied() {
        let p = policy();
        for dst in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "192.168.127.2",
            "169.254.169.254",
            "100.100.100.100",
            "224.0.0.251",
            "239.255.255.250",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "45.33.0.1",
            "45.33.0.7",
        ] {
            assert!(
                matches!(p.check(&tcp(dst, 443)), Err(Denial::Denied { .. })),
                "{dst}"
            );
        }
    }

    #[test]
    fn public_internet_is_allowed_except_smtp() {
        let p = policy();
        for (dst, port) in [("1.1.1.1", 443), ("8.8.8.8", 53), ("93.184.215.14", 80)] {
            assert_eq!(p.check(&tcp(dst, port)), Ok(()), "{dst}:{port}");
            assert_eq!(p.check(&udp(dst, port)), Ok(()), "{dst}:{port}");
        }
        assert_eq!(p.check(&ipv4("1.1.1.1".parse().unwrap(), 1, 0, 0)), Ok(()));
        for port in SMTP_PORTS {
            assert!(matches!(
                p.check(&tcp("1.1.1.1", port)),
                Err(Denial::Smtp { .. })
            ));
            // UDP 25 is not SMTP.
            assert_eq!(p.check(&udp("1.1.1.1", port)), Ok(()));
        }
        // 172.15/172.32 are outside 172.16.0.0/12.
        assert_eq!(p.check(&tcp("172.15.0.1", 443)), Ok(()));
        assert_eq!(p.check(&tcp("172.32.0.1", 443)), Ok(()));
    }

    #[test]
    fn only_ipv4_and_arp() {
        let p = policy();
        assert_eq!(p.check(&eth(ETHERTYPE_ARP, &[0; 28])), Ok(()));
        for t in [0x86dd, 0x8100, 0x88a8, 0x0842, 0x0000] {
            assert_eq!(p.check(&eth(t, &[0; 40])), Err(Denial::EtherType(t)));
        }
        // VLAN-tagged IPv4 to loopback: refused by ethertype, never parsed.
        let mut tagged = eth(0x8100, &[0, 1, 0x08, 0x00]);
        tagged.extend_from_slice(&tcp("127.0.0.1", 80)[14..]);
        assert!(matches!(p.check(&tagged), Err(Denial::EtherType(0x8100))));
    }

    #[test]
    fn malformed_frames_are_dropped() {
        let p = policy();
        assert_eq!(p.check(&[0; 13]), Err(Denial::Malformed));
        assert_eq!(
            p.check(&eth(ETHERTYPE_IPV4, &[0x45; 10])),
            Err(Denial::Malformed)
        );
        let mut f = tcp("1.1.1.1", 443);
        f[14] = 0x65; // version 6 in an IPv4 frame
        assert_eq!(p.check(&f), Err(Denial::Malformed));
        let mut f = tcp("1.1.1.1", 443);
        f[14] = 0x44; // IHL 16 bytes
        assert_eq!(p.check(&f), Err(Denial::Malformed));
        let mut f = tcp("1.1.1.1", 443);
        f[16..18].copy_from_slice(&9000u16.to_be_bytes()); // total > frame
        assert_eq!(p.check(&f), Err(Denial::Malformed));
        // IP options shift the L4 header: the port is read after them.
        let mut f = tcp("1.1.1.1", 25);
        f[14] = 0x46;
        f.splice(34..34, [1, 1, 1, 1]);
        let total = u16::try_from(f.len() - 14).unwrap();
        f[16..18].copy_from_slice(&total.to_be_bytes());
        assert!(matches!(p.check(&f), Err(Denial::Smtp { port: 25, .. })));
    }

    #[test]
    fn prefixes_parse_and_match() {
        let n: Ipv4Net = "10.0.0.0/8".parse().unwrap();
        assert!(n.contains("10.255.0.1".parse().unwrap()));
        assert!(!n.contains("11.0.0.0".parse().unwrap()));
        assert!("10.0.0.1/8".parse::<Ipv4Net>().is_err(), "host bits");
        assert!("10.0.0.0/33".parse::<Ipv4Net>().is_err());
        assert!("10.0.0.0".parse::<Ipv4Net>().is_err());
        let all: Ipv4Net = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(Ipv4Addr::BROADCAST));
        assert_eq!(
            Ipv4Net::new("1.2.3.4".parse().unwrap(), 24)
                .unwrap()
                .to_string(),
            "1.2.3.0/24"
        );
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"10.0.0.0/8\"");
        assert_eq!(serde_json::from_str::<Ipv4Net>(&json).unwrap(), n);
        assert_eq!(always_denied().len(), ALWAYS_DENIED.len());
    }

    #[test]
    fn host_prefixes_cover_own_addresses_but_not_loopback() {
        let p = host_prefixes().unwrap();
        assert!(p.iter().all(|n| n.prefix() >= 8), "{p:?}");
        assert!(!p.iter().any(|n| n.contains(Ipv4Addr::LOCALHOST)), "{p:?}");
        // Every own address is denied, by the host list or the built-in ranges.
        let policy = EgressPolicy::new(&p);
        for ifa in nix::ifaddrs::getifaddrs().unwrap() {
            if let Some(a) = ifa.address.as_ref().and_then(|a| a.as_sockaddr_in())
                && !a.ip().is_loopback()
            {
                let f = frames::tcp(&a.ip().to_string(), 22);
                assert!(
                    policy.check(&f).is_err(),
                    "own address {} reachable",
                    a.ip()
                );
            }
        }
    }

    #[test]
    fn host_prefixes_are_minimal() {
        let n = |s: &str| s.parse::<Ipv4Net>().unwrap();
        let mut taps: Vec<Ipv4Net> = (0..64)
            .flat_map(|i| {
                [
                    n(&format!("172.30.{i}.0/30")),
                    n(&format!("172.30.{i}.1/32")),
                ]
            })
            .collect();
        taps.extend([
            n("51.0.0.0/24"),
            n("51.0.0.7/32"),
            n("51.0.0.7/32"),
            n("10.1.2.3/32"),
        ]);
        assert_eq!(minimize(taps), [n("51.0.0.0/24")]);
    }

    #[test]
    fn argv_is_locked_down() {
        let passt = NetstackKind::Passt.args().join(" ");
        for want in [
            "--fd 0",
            "--foreground",
            "--ipv4-only",
            "--no-map-gw",
            "-t none",
            "-u none",
            "--address 192.168.127.2",
            "--gateway 192.168.127.1",
            "--dns-forward 192.168.127.1",
            "--mtu 1500",
            "--no-dhcp-dns",
            "--no-dhcp-search",
        ] {
            assert!(passt.contains(want), "passt: missing {want}: {passt}");
        }
        for never in [
            "-T",
            "-U",
            "--map-host-loopback",
            "--socket",
            "-s ",
            "--conf-path",
        ] {
            assert!(!passt.contains(never), "passt: {never}");
        }
        let gv = NetstackKind::Gvproxy.args().join(" ");
        assert_eq!(gv, "-listen-stdio accept -ssh-port -1 -mtu 1500");
        for never in [
            "-listen ",
            "-services",
            "-forward-sock",
            "-ec2-metadata-access",
            "-listen-qemu",
        ] {
            assert!(!gv.contains(never), "gvproxy: {never}");
        }
        assert_eq!(NetstackKind::Passt.framing(), Framing::Be32);
        assert_eq!(NetstackKind::Gvproxy.framing(), Framing::Le16);
    }

    #[test]
    fn framing_round_trips_and_bounds() {
        for fr in [Framing::Be32, Framing::Le16] {
            let mut wire = Vec::new();
            fr.write_frame(&mut wire, b"hello").unwrap();
            fr.write_frame(&mut wire, &[7; 1514]).unwrap();
            let mut r = io::Cursor::new(wire);
            let mut buf = Vec::new();
            assert!(fr.read_frame(&mut r, &mut buf).unwrap());
            assert_eq!(buf, b"hello");
            assert!(fr.read_frame(&mut r, &mut buf).unwrap());
            assert_eq!(buf.len(), 1514);
            assert!(!fr.read_frame(&mut r, &mut buf).unwrap(), "clean EOF");
        }
        // Truncated header / body.
        let mut buf = Vec::new();
        assert!(
            Framing::Be32
                .read_frame(&mut io::Cursor::new(vec![0, 0]), &mut buf)
                .is_err()
        );
        assert!(
            Framing::Be32
                .read_frame(&mut io::Cursor::new(vec![0, 0, 0, 9, 1]), &mut buf)
                .is_err()
        );
        // Oversized.
        let big = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        assert!(
            Framing::Be32
                .read_frame(&mut io::Cursor::new(big.to_vec()), &mut buf)
                .is_err()
        );
        assert!(
            Framing::Le16
                .write_frame(&mut Vec::new(), &vec![0; 70_000])
                .is_err()
        );
    }

    /// The filter between two socketpairs: allowed frames come out re-framed, denied ones never
    /// reach the netstack side, and replies flow back; closing the guest side ends both.
    #[test]
    fn shuttles_filter_and_reframe() {
        let (guest_vm, guest_helper) = UnixStream::pair().unwrap();
        let (ns_helper, ns_peer) = UnixStream::pair().unwrap();
        let policy = EgressPolicy::new(&[]);
        let stats = Arc::new(Stats::default());
        let logs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (gh2, nh2) = (
            guest_helper.try_clone().unwrap(),
            ns_helper.try_clone().unwrap(),
        );
        let (s1, s2, l1) = (Arc::clone(&stats), Arc::clone(&stats), Arc::clone(&logs));
        let eg = std::thread::spawn(move || {
            let log = move |m: String| l1.lock().unwrap().push(m);
            shuttle_egress(guest_helper, ns_helper, Framing::Le16, &policy, &s1, &log)
        });
        let ing = std::thread::spawn(move || shuttle_ingress(nh2, gh2, Framing::Le16, &s2));

        let mut vm = &guest_vm;
        for f in [
            tcp("127.0.0.1", 22),
            tcp("192.168.127.254", 8080),
            tcp("192.168.127.1", 80),
            udp("192.168.127.1", 53),
            tcp("1.1.1.1", 443),
        ] {
            Framing::Be32.write_frame(&mut vm, &f).unwrap();
        }
        // Netstack side sees exactly the two allowed frames, in order, 2-byte framed.
        let mut r = BufReader::new(&ns_peer);
        let mut buf = Vec::new();
        assert!(Framing::Le16.read_frame(&mut r, &mut buf).unwrap());
        assert_eq!(buf, udp("192.168.127.1", 53));
        assert!(Framing::Le16.read_frame(&mut r, &mut buf).unwrap());
        assert_eq!(buf, tcp("1.1.1.1", 443));
        // A reply comes back 4-byte framed.
        Framing::Le16
            .write_frame(&mut &ns_peer, b"reply-frame-xx")
            .unwrap();
        let mut rv = BufReader::new(&guest_vm);
        assert!(Framing::Be32.read_frame(&mut rv, &mut buf).unwrap());
        assert_eq!(buf, b"reply-frame-xx");

        guest_vm.shutdown(Shutdown::Write).unwrap();
        eg.join().unwrap().unwrap();
        ing.join().unwrap().ok();
        assert!(
            !Framing::Le16.read_frame(&mut r, &mut buf).unwrap(),
            "netstack sees EOF"
        );
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 3);
        assert_eq!(stats.to_netstack.load(Ordering::Relaxed), 2);
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 3, "{logs:?}");
        assert!(
            logs[0].contains("127.0.0.1 is in denied 127.0.0.0/8"),
            "{logs:?}"
        );
    }
}
