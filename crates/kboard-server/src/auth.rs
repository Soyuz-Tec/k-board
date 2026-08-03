//! The standalone deployment's authority.
//!
//! One seam, deliberately. A host embedding the engine replaces this whole
//! binary with its own membership check and never reaches any of it — which
//! only stays true if the check is one place rather than scattered through the
//! request path.
//!
//! ## Fail closed where it matters
//!
//! The server binds loopback by default, so an unauthenticated board has never
//! been reachable from another machine. The dangerous configuration is not
//! "no secret", it is "no secret *and* a public bind" — so that is the
//! combination refused at startup, rather than forcing a secret on someone
//! running the quickstart on their own machine.
//!
//! ## Tokens
//!
//! `base64url(payload).base64url(hmac-sha256(payload))`, where the payload names
//! the scope the bearer may open and when the grant expires. Self-contained, so
//! verifying one needs no lookup and no shared state between processes.
//!
//! A token authorises **one scope**. A bearer for `tenant-a/board` cannot open
//! `tenant-b/board`, which is the property that makes the scope in the URL
//! untrusted input rather than an authorisation decision.

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Default grant lifetime when minting.
pub const DEFAULT_TTL_SECONDS: u64 = 12 * 60 * 60;

#[derive(Serialize, Deserialize)]
struct Claims {
    /// The one scope this token opens.
    scope: String,
    /// Unix seconds after which the grant is refused.
    exp: u64,
}

/// Why a connection was refused. Never returned to the caller in detail — a
/// client that learns *why* its token failed learns something about the secret.
#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    Missing,
    Malformed,
    BadSignature,
    Expired,
    WrongScope,
}

pub struct Authority {
    secret: Option<Vec<u8>>,
}

impl Authority {
    /// Reads `KBOARD_SECRET`. Absent means the server runs open, which is only
    /// permitted on a loopback bind — see [`Authority::permits_bind`].
    pub fn from_env() -> Self {
        let secret = std::env::var("KBOARD_SECRET")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(String::into_bytes);
        Self { secret }
    }

    #[cfg(test)]
    fn with_secret(secret: &str) -> Self {
        Self {
            secret: Some(secret.as_bytes().to_vec()),
        }
    }

    #[cfg(test)]
    fn open() -> Self {
        Self { secret: None }
    }

    pub fn is_enforcing(&self) -> bool {
        self.secret.is_some()
    }

    /// Whether this authority may serve on `address`.
    ///
    /// An open server on loopback is a development convenience. An open server
    /// on any other interface is an unauthenticated writable store on a
    /// network, which is not a configuration to warn about — it is one to
    /// refuse.
    pub fn permits_bind(&self, address: IpAddr) -> bool {
        self.is_enforcing() || address.is_loopback()
    }

    /// Issue a token for one scope.
    ///
    /// Returns `None` when no secret is configured: minting a token an open
    /// server would ignore invites someone to believe it is protecting them.
    pub fn mint(&self, scope: &str, ttl_seconds: u64) -> Option<String> {
        let secret = self.secret.as_ref()?;
        let claims = Claims {
            scope: scope.to_owned(),
            exp: now_seconds().saturating_add(ttl_seconds),
        };
        let payload = serde_json::to_vec(&claims).ok()?;
        let encoded = URL_SAFE_NO_PAD.encode(&payload);
        let signature = sign(secret, encoded.as_bytes());
        Some(format!("{encoded}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// Whether `token` authorises opening `scope` right now.
    ///
    /// # Errors
    ///
    /// A [`Denied`] reason, for logging only. Callers must not relay it.
    pub fn verify(&self, token: Option<&str>, scope: &str) -> Result<(), Denied> {
        let Some(secret) = self.secret.as_ref() else {
            return Ok(());
        };
        let token = token.ok_or(Denied::Missing)?;
        let (encoded, signature) = token.split_once('.').ok_or(Denied::Malformed)?;

        let presented = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| Denied::Malformed)?;
        // Verified before the payload is even parsed, and with a constant-time
        // comparison: an attacker must not learn how much of a forged
        // signature was right, nor reach the parser with an unsigned payload.
        let expected = sign(secret, encoded.as_bytes());
        if presented.len() != expected.len() || !bool::from(constant_time_eq(&presented, &expected))
        {
            return Err(Denied::BadSignature);
        }

        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| Denied::Malformed)?;
        let claims: Claims = serde_json::from_slice(&payload).map_err(|_| Denied::Malformed)?;

        if claims.exp <= now_seconds() {
            return Err(Denied::Expired);
        }
        if claims.scope != scope {
            return Err(Denied::WrongScope);
        }
        Ok(())
    }
}

fn sign(secret: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// Compares without an early exit, so timing does not reveal a prefix match.
fn constant_time_eq(left: &[u8], right: &[u8]) -> subtle::Choice {
    use subtle::ConstantTimeEq;
    left.ct_eq(right)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn a_minted_token_opens_its_own_scope() {
        let authority = Authority::with_secret("correct horse battery staple");
        let token = authority
            .mint("tenant-a/board", DEFAULT_TTL_SECONDS)
            .unwrap();
        assert_eq!(authority.verify(Some(&token), "tenant-a/board"), Ok(()));
    }

    #[test]
    fn a_token_does_not_open_another_scope() {
        let authority = Authority::with_secret("secret");
        let token = authority
            .mint("tenant-a/board", DEFAULT_TTL_SECONDS)
            .unwrap();
        // This is what makes the scope in the URL untrusted input rather than
        // an authorisation decision.
        assert_eq!(
            authority.verify(Some(&token), "tenant-b/board"),
            Err(Denied::WrongScope)
        );
    }

    #[test]
    fn a_token_from_another_secret_is_refused() {
        let issuer = Authority::with_secret("one secret");
        let verifier = Authority::with_secret("a different secret");
        let token = issuer.mint("t/b", DEFAULT_TTL_SECONDS).unwrap();
        assert_eq!(
            verifier.verify(Some(&token), "t/b"),
            Err(Denied::BadSignature)
        );
    }

    #[test]
    fn an_expired_token_is_refused() {
        let authority = Authority::with_secret("secret");
        let token = authority.mint("t/b", 0).unwrap();
        assert_eq!(authority.verify(Some(&token), "t/b"), Err(Denied::Expired));
    }

    #[test]
    fn a_tampered_payload_is_refused_before_it_is_parsed() {
        let authority = Authority::with_secret("secret");
        let token = authority.mint("t/b", DEFAULT_TTL_SECONDS).unwrap();
        let (_, signature) = token.split_once('.').unwrap();

        // A payload claiming a different scope, carrying the original
        // signature. Rewriting claims must fail on the signature.
        let forged_payload =
            URL_SAFE_NO_PAD.encode(br#"{"scope":"other/board","exp":99999999999}"#);
        let forged = format!("{forged_payload}.{signature}");
        assert_eq!(
            authority.verify(Some(&forged), "other/board"),
            Err(Denied::BadSignature)
        );
    }

    #[test]
    fn malformed_tokens_are_refused_rather_than_panicking() {
        let authority = Authority::with_secret("secret");
        for candidate in ["", ".", "no-dot", "!!!.!!!", "a.b"] {
            assert!(
                authority.verify(Some(candidate), "t/b").is_err(),
                "{candidate}"
            );
        }
        assert_eq!(authority.verify(None, "t/b"), Err(Denied::Missing));
    }

    #[test]
    fn an_open_authority_admits_everyone_and_mints_nothing() {
        let authority = Authority::open();
        assert!(!authority.is_enforcing());
        assert_eq!(authority.verify(None, "t/b"), Ok(()));
        // Handing back a token an open server ignores would invite someone to
        // believe it protects them.
        assert!(authority.mint("t/b", DEFAULT_TTL_SECONDS).is_none());
    }

    #[test]
    fn an_open_server_may_only_bind_loopback() {
        let open = Authority::open();
        assert!(open.permits_bind(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(open.permits_bind(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // An unauthenticated writable store on a network is not a warning.
        assert!(!open.permits_bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!open.permits_bind(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
    }

    #[test]
    fn an_enforcing_server_may_bind_anywhere() {
        let guarded = Authority::with_secret("secret");
        assert!(guarded.permits_bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    }
}
