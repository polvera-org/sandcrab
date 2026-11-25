//! `auth`: per-connection shared-secret authentication.
//!
//! Transports that any local process can reach (TCP on `127.0.0.1`, e.g. the Docker backend
//! on Docker Desktop) need more than file permissions. When `crabd` is started with a token
//! (`crabd serve --auth-token-file <path>`), **every** connection must open with exactly one
//! `auth` frame carrying it, before `hello`, any tool call or `tunnel.open`:
//!
//! ```text
//! -> {"jsonrpc":"2.0","id":0,"method":"auth","params":{"token":"<64 hex chars>"}}
//! <- {"jsonrpc":"2.0","id":0,"result":{}}
//! ... then the connection is an ordinary one (frames, or tunnel.open as the next frame) ...
//! ```
//!
//! On a wrong, missing or malformed token, or when no frame arrives in time, `crabd` answers
//! one [`ProtocolError::Unauthorized`] frame (no detail about why) and closes the connection
//! without serving anything. The comparison is constant-time.
//!
//! The token is an [`AuthToken`]: [`AUTH_TOKEN_BYTES`] bytes (256 bits) from the host's OS
//! CSPRNG, hex-encoded. Its `Debug` output is redacted so it never ends up in logs or error
//! messages. Like `tunnel.open`, `auth` is a connection-level verb: it is not a tool and not
//! part of [`crate::Call`].

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ProtocolError, RequestId, RpcRequest,
    tools::{from_params, to_json},
};

/// Method of the connection-opening authentication frame.
pub const METHOD_AUTH: &str = "auth";

/// Raw token size: 256 bits.
pub const AUTH_TOKEN_BYTES: usize = 32;

/// Length of the hex-encoded token on the wire and on disk.
pub const AUTH_TOKEN_HEX_LEN: usize = AUTH_TOKEN_BYTES * 2;

/// A per-sandbox shared secret, hex-encoded (lowercase).
///
/// `Debug` is redacted and there is no `Display`: use [`AuthToken::expose`] at the single
/// place the secret must be written (the token file, the `auth` frame).
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Encode raw random bytes (the caller supplies them from the OS CSPRNG).
    #[must_use]
    pub fn from_bytes(bytes: &[u8; AUTH_TOKEN_BYTES]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(AUTH_TOKEN_HEX_LEN);
        for b in bytes {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        Self(s)
    }

    /// Parse a token file / wire value: surrounding whitespace is ignored, the rest must be
    /// exactly [`AUTH_TOKEN_HEX_LEN`] hex digits (case-insensitive, stored lowercase).
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        let s = s.trim();
        if s.len() != AUTH_TOKEN_HEX_LEN || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtocolError::InvalidParams(format!(
                "auth token must be {AUTH_TOKEN_HEX_LEN} hex digits"
            )));
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    /// The secret itself. Never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented token (time depends only on the
    /// lengths, which are public). Case-insensitive, like [`Self::parse`].
    #[must_use]
    pub fn verify(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(presented) {
            diff |= a ^ b.to_ascii_lowercase();
        }
        std::hint::black_box(diff) == 0
    }

    /// The `auth` request for this token.
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(
            id,
            METHOD_AUTH,
            Some(to_json(&AuthParams {
                token: self.0.clone(),
            })),
        )
    }
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

/// `auth` params.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthParams {
    /// The hex token. Redacted in `Debug`.
    pub token: String,
}

impl fmt::Debug for AuthParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthParams")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl AuthParams {
    /// Decode from an `auth` request. Any other method is [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_AUTH {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        from_params(req.params.clone().unwrap_or(Value::Null))
    }
}

/// `auth` result: an empty object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthAck {}

impl AuthAck {
    #[must_use]
    pub fn to_value(&self) -> Value {
        to_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Call, RpcMessage, encode_frame};
    use serde_json::json;

    fn token() -> AuthToken {
        let mut b = [0u8; AUTH_TOKEN_BYTES];
        for (i, x) in b.iter_mut().enumerate() {
            *x = u8::try_from(i * 7 % 256).unwrap();
        }
        AuthToken::from_bytes(&b)
    }

    #[test]
    fn hex_encoding_and_parse() {
        let t = AuthToken::from_bytes(&[0xab; AUTH_TOKEN_BYTES]);
        assert_eq!(t.expose(), "ab".repeat(AUTH_TOKEN_BYTES));
        assert_eq!(AuthToken::parse(&format!("  {}\n", t.expose())).unwrap(), t);
        assert_eq!(
            AuthToken::parse(&"AB".repeat(AUTH_TOKEN_BYTES)).unwrap(),
            t,
            "upper case accepted"
        );
        for bad in [
            "",
            "abc",
            &"ab".repeat(31),
            &"zz".repeat(32),
            &"ab".repeat(33),
        ] {
            assert!(AuthToken::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn verify_is_exact() {
        let t = token();
        assert!(t.verify(t.expose()));
        assert!(t.verify(&t.expose().to_ascii_uppercase()));
        assert!(!t.verify(""));
        assert!(!t.verify(&t.expose()[1..]));
        assert!(!t.verify(&format!("{}0", t.expose())));
        let mut flipped = t.expose().to_owned().into_bytes();
        flipped[63] = if flipped[63] == b'0' { b'1' } else { b'0' };
        assert!(!t.verify(std::str::from_utf8(&flipped).unwrap()));
        assert!(!t.verify(&"0".repeat(AUTH_TOKEN_HEX_LEN)));
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let t = token();
        let req = t.to_request(0);
        let p = AuthParams::from_request(&req).unwrap();
        for s in [format!("{t:?}"), format!("{p:?}")] {
            assert!(!s.contains(t.expose()), "{s}");
            assert!(s.contains("redacted"), "{s}");
        }
    }

    #[test]
    fn request_round_trip() {
        let t = token();
        let req = t.to_request(0);
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"jsonrpc":"2.0","id":0,"method":"auth","params":{"token": t.expose()}})
        );
        let frame = encode_frame(&req).unwrap();
        let RpcMessage::Request(back) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
            panic!("request")
        };
        assert_eq!(AuthParams::from_request(&back).unwrap().token, t.expose());
        assert_eq!(AuthAck::default().to_value(), json!({}));
    }

    #[test]
    fn malformed_params_and_other_methods_rejected() {
        for params in [
            json!({}),
            json!(null),
            json!({"token": 5}),
            json!({"token":"a","x":1}),
        ] {
            let req = RpcRequest::new(0, METHOD_AUTH, Some(params.clone()));
            assert!(AuthParams::from_request(&req).is_err(), "{params}");
        }
        let req = RpcRequest::new(0, "hello", Some(json!({"token": "x"})));
        assert!(matches!(
            AuthParams::from_request(&req),
            Err(ProtocolError::MethodNotFound(_))
        ));
        assert!(!crate::TOOL_NAMES.contains(&METHOD_AUTH));
        assert!(Call::from_method_params(METHOD_AUTH, json!({"token": "x"})).is_err());
    }
}
