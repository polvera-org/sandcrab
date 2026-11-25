//! Minimal rtnetlink: bring an interface up with one `RTM_NEWLINK` request.
//!
//! Hand-encoded (32 bytes) instead of pulling `netlink-packet-route` + `netlink-sys` into the
//! static guest binary for a single message. Encoding / ack parsing are pure and unit-tested;
//! the socket I/O is Linux-only.

const NLMSG_HDRLEN: usize = 16;
const IFINFOMSG_LEN: usize = 16;
const RTM_NEWLINK: u16 = 16;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const IFF_UP: u32 = 0x1;

/// `RTM_NEWLINK` setting `IFF_UP` on interface `index` (native endian, as netlink requires).
#[must_use]
pub fn encode_link_up(index: u32, seq: u32) -> [u8; NLMSG_HDRLEN + IFINFOMSG_LEN] {
    let mut m = [0u8; NLMSG_HDRLEN + IFINFOMSG_LEN];
    let len = u32::try_from(m.len()).expect("fixed size");
    m[0..4].copy_from_slice(&len.to_ne_bytes());
    m[4..6].copy_from_slice(&RTM_NEWLINK.to_ne_bytes());
    m[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
    m[8..12].copy_from_slice(&seq.to_ne_bytes());
    // nlmsg_pid = 0 (kernel assigns); ifinfomsg: family AF_UNSPEC, pad, type 0.
    m[20..24].copy_from_slice(&index.to_ne_bytes()); // ifi_index (i32; ifindex > 0)
    m[24..28].copy_from_slice(&IFF_UP.to_ne_bytes()); // ifi_flags
    m[28..32].copy_from_slice(&IFF_UP.to_ne_bytes()); // ifi_change
    m
}

/// Parse the kernel's `NLMSG_ERROR` ack for `seq`: `Ok(())` for error 0, `Err(errno)` else.
pub fn parse_ack(buf: &[u8], seq: u32) -> Result<(), String> {
    if buf.len() < NLMSG_HDRLEN + 4 {
        return Err(format!("short netlink reply ({} bytes)", buf.len()));
    }
    let ty = u16::from_ne_bytes([buf[4], buf[5]]);
    let got_seq = u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if ty != NLMSG_ERROR {
        return Err(format!("unexpected netlink message type {ty}"));
    }
    if got_seq != seq {
        return Err(format!("netlink ack for seq {got_seq}, expected {seq}"));
    }
    let err = i32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]);
    if err == 0 {
        Ok(())
    } else {
        Err(format!("netlink error {}", -err))
    }
}

#[cfg(target_os = "linux")]
pub fn link_up(name: &str) -> std::io::Result<()> {
    use rustix::net::{
        AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, netdevice, netlink, recv,
        sendto, socket, socket_with,
    };
    let index = {
        let probe = socket(AddressFamily::INET, SocketType::DGRAM, None)?;
        netdevice::name_to_index(&probe, name)?
    };
    let fd = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let seq = 1;
    sendto(
        &fd,
        &encode_link_up(index, seq),
        SendFlags::empty(),
        &netlink::SocketAddrNetlink::new(0, 0),
    )?;
    let mut buf = [0u8; 512];
    let (n, _) = recv(&fd, &mut buf, RecvFlags::empty())?;
    parse_ack(&buf[..n], seq).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_link_up() {
        let m = encode_link_up(1, 7);
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), 32);
        assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_NEWLINK);
        assert_eq!(u16::from_ne_bytes(m[6..8].try_into().unwrap()), 0x5);
        assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 7);
        assert_eq!(m[16], 0, "AF_UNSPEC");
        assert_eq!(i32::from_ne_bytes(m[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(m[24..28].try_into().unwrap()), IFF_UP);
        assert_eq!(u32::from_ne_bytes(m[28..32].try_into().unwrap()), IFF_UP);
    }

    fn ack(seq: u32, err: i32) -> Vec<u8> {
        let mut b = vec![0u8; 36];
        b[0..4].copy_from_slice(&36u32.to_ne_bytes());
        b[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        b[8..12].copy_from_slice(&seq.to_ne_bytes());
        b[16..20].copy_from_slice(&err.to_ne_bytes());
        b
    }

    #[test]
    fn parses_acks() {
        assert_eq!(parse_ack(&ack(1, 0), 1), Ok(()));
        assert!(parse_ack(&ack(1, -1), 1).unwrap_err().contains("error 1"));
        assert!(parse_ack(&ack(2, 0), 1).is_err());
        assert!(parse_ack(&[0; 4], 1).is_err());
    }

    /// Real kernel round trip: as root `lo` goes up; unprivileged the kernel parses the
    /// request and answers EPERM, which still proves the encoding and ack parsing.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_accepts_request() {
        match link_up("lo") {
            Ok(()) => {}
            Err(e) => assert!(e.to_string().contains("netlink error 1"), "{e}"),
        }
    }
}
