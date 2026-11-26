//! Who is on the other end of a connection (design §5.8): the kernel's peer credentials
//! (`SO_PEERCRED` on Linux, `getpeereid` on macOS, both through Tokio), checked against the
//! allowed uids before the first frame is read.

use tokio::net::UnixStream;

/// The peer of one connection, as the host kernel sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
}

impl Peer {
    /// Read the peer credentials of `stream`.
    pub fn of(stream: &UnixStream) -> std::io::Result<Self> {
        let cred = stream.peer_cred()?;
        Ok(Self {
            uid: cred.uid(),
            gid: cred.gid(),
        })
    }

    /// Whether this peer is admitted.
    #[must_use]
    pub fn admitted(&self, allow_uids: &[u32]) -> bool {
        allow_uids.contains(&self.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_own_credentials_and_checks_the_list() {
        let (a, _b) = UnixStream::pair().unwrap();
        let p = Peer::of(&a).unwrap();
        let me = sandcrab_sdk::core::private_dir::current_uid();
        assert_eq!(p.uid, me);
        assert!(p.admitted(&[me]));
        assert!(p.admitted(&[me.wrapping_add(1), me]));
        assert!(!p.admitted(&[me.wrapping_add(1)]));
        assert!(!p.admitted(&[]));
    }
}
