//! Browser trust-boundary and aggregate admission controls.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};

use crate::limits::{self, RateLimiter};

#[derive(Clone, Debug)]
pub struct OriginPolicy {
    allowed: Arc<HashSet<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginDenied {
    Malformed,
    NotAllowed,
}

impl OriginPolicy {
    pub fn from_env(bind: IpAddr, port: u16) -> Result<Self, &'static str> {
        let configured = std::env::var("KBOARD_ALLOWED_ORIGINS").unwrap_or_default();
        let mut allowed = configured
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(normalize_origin)
            .collect::<Result<HashSet<_>, _>>()?;

        if bind.is_loopback() && allowed.is_empty() {
            allowed.extend([
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{port}"),
                format!("http://[::1]:{port}"),
                format!("https://127.0.0.1:{port}"),
                format!("https://localhost:{port}"),
                format!("https://[::1]:{port}"),
            ]);
        }
        if !bind.is_loopback() && allowed.is_empty() {
            return Err("KBOARD_ALLOWED_ORIGINS is required for a non-loopback bind");
        }
        Ok(Self {
            allowed: Arc::new(allowed),
        })
    }

    pub fn check(&self, headers: &HeaderMap) -> Result<(), OriginDenied> {
        let Some(origin) = headers.get(header::ORIGIN) else {
            // Non-browser clients have no ambient browser authority. They are
            // still subject to bearer verification and every resource limit.
            return Ok(());
        };
        let origin = origin.to_str().map_err(|_| OriginDenied::Malformed)?;
        let normalized = normalize_origin(origin).map_err(|_| OriginDenied::Malformed)?;
        if self.allowed.contains(&normalized) {
            Ok(())
        } else {
            Err(OriginDenied::NotAllowed)
        }
    }
}

fn normalize_origin(origin: &str) -> Result<String, &'static str> {
    let trimmed = origin.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.contains([' ', '\n', '\r', '\t'])
        || !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        || trimmed[trimmed.find("://").unwrap_or(0) + 3..].contains('/')
    {
        return Err("origin must be an absolute HTTP(S) origin without a path");
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Fixed-cardinality correlation suitable for logs and metrics.
pub fn scope_correlation(scope: &str) -> String {
    let digest = Sha256::digest(scope.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone)]
pub struct AdmissionControl {
    inner: Arc<Mutex<AdmissionState>>,
}

struct AdmissionState {
    total_connections: usize,
    scopes: HashMap<String, ScopeBudget>,
    identities: HashMap<String, IdentityBudget>,
    replicas: HashMap<(String, String), u64>,
}

struct ScopeBudget {
    connections: usize,
    limiter: RateLimiter,
}

struct IdentityBudget {
    scopes: HashSet<String>,
    limiter: RateLimiter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    ProcessFull,
    ScopeFull,
    ReplicaInUse,
    IdentityScopeLimit,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct AdmissionStats {
    pub connections: usize,
    pub scopes: usize,
    pub identities: usize,
    pub claimed_replicas: usize,
}

pub struct ConnectionClaim {
    control: AdmissionControl,
    scope: String,
}

pub struct ReplicaClaim {
    control: AdmissionControl,
    scope: String,
    replica: String,
    connection: u64,
}

impl AdmissionControl {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AdmissionState {
                total_connections: 0,
                scopes: HashMap::new(),
                identities: HashMap::new(),
                replicas: HashMap::new(),
            })),
        }
    }

    pub fn claim_connection(&self, scope: &str) -> Result<ConnectionClaim, AdmissionError> {
        let mut state = self.lock();
        if state.total_connections >= limits::MAX_CONNECTIONS {
            return Err(AdmissionError::ProcessFull);
        }
        let scope_budget = state
            .scopes
            .entry(scope.to_owned())
            .or_insert_with(|| ScopeBudget {
                connections: 0,
                limiter: RateLimiter::with_budget(
                    limits::SCOPE_RATE_PER_SECOND,
                    limits::SCOPE_RATE_BURST,
                ),
            });
        if scope_budget.connections >= limits::MAX_CONNECTIONS_PER_SCOPE {
            return Err(AdmissionError::ScopeFull);
        }
        scope_budget.connections += 1;
        state.total_connections += 1;
        Ok(ConnectionClaim {
            control: self.clone(),
            scope: scope.to_owned(),
        })
    }

    pub fn claim_replica(
        &self,
        scope: &str,
        replica: &str,
        connection: u64,
    ) -> Result<ReplicaClaim, AdmissionError> {
        let mut state = self.lock();
        let key = (scope.to_owned(), replica.to_owned());
        if state.replicas.contains_key(&key) {
            return Err(AdmissionError::ReplicaInUse);
        }
        let identity = state
            .identities
            .entry(replica.to_owned())
            .or_insert_with(|| IdentityBudget {
                scopes: HashSet::new(),
                limiter: RateLimiter::with_budget(
                    limits::IDENTITY_RATE_PER_SECOND,
                    limits::IDENTITY_RATE_BURST,
                ),
            });
        if !identity.scopes.contains(scope)
            && identity.scopes.len() >= limits::MAX_SCOPES_PER_IDENTITY
        {
            return Err(AdmissionError::IdentityScopeLimit);
        }
        identity.scopes.insert(scope.to_owned());
        state.replicas.insert(key, connection);
        Ok(ReplicaClaim {
            control: self.clone(),
            scope: scope.to_owned(),
            replica: replica.to_owned(),
            connection,
        })
    }

    pub fn allow_frame(&self, scope: &str, replica: Option<&str>) -> bool {
        let mut state = self.lock();
        let scope_allowed = state
            .scopes
            .get_mut(scope)
            .is_some_and(|budget| budget.limiter.allow());
        let identity_allowed = replica.is_none_or(|replica| {
            state
                .identities
                .get_mut(replica)
                .is_some_and(|budget| budget.limiter.allow())
        });
        scope_allowed && identity_allowed
    }

    pub fn stats(&self) -> AdmissionStats {
        let state = self.lock();
        AdmissionStats {
            connections: state.total_connections,
            scopes: state.scopes.len(),
            identities: state.identities.len(),
            claimed_replicas: state.replicas.len(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AdmissionState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for AdmissionControl {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConnectionClaim {
    fn drop(&mut self) {
        let mut state = self.control.lock();
        state.total_connections = state.total_connections.saturating_sub(1);
        if let Some(scope) = state.scopes.get_mut(&self.scope) {
            scope.connections = scope.connections.saturating_sub(1);
            if scope.connections == 0 {
                state.scopes.remove(&self.scope);
            }
        }
    }
}

impl Drop for ReplicaClaim {
    fn drop(&mut self) {
        let mut state = self.control.lock();
        let key = (self.scope.clone(), self.replica.clone());
        if state.replicas.get(&key) == Some(&self.connection) {
            state.replicas.remove(&key);
            let still_active = state
                .replicas
                .keys()
                .any(|(scope, replica)| scope == &self.scope && replica == &self.replica);
            if !still_active {
                if let Some(identity) = state.identities.get_mut(&self.replica) {
                    identity.scopes.remove(&self.scope);
                    if identity.scopes.is_empty() {
                        state.identities.remove(&self.replica);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_are_exact_and_extension_offers_do_not_change_origin_policy() {
        let policy = OriginPolicy {
            allowed: Arc::new(HashSet::from(["https://board.example".to_owned()])),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://board.example".parse().unwrap());
        assert_eq!(policy.check(&headers), Ok(()));
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert_eq!(policy.check(&headers), Err(OriginDenied::NotAllowed));
        headers.insert(
            header::SEC_WEBSOCKET_EXTENSIONS,
            "permessage-deflate".parse().unwrap(),
        );
        assert_eq!(policy.check(&headers), Err(OriginDenied::NotAllowed));
        headers.insert(header::ORIGIN, "https://board.example".parse().unwrap());
        assert_eq!(policy.check(&headers), Ok(()));
    }

    #[test]
    fn many_connections_cannot_multiply_one_scopes_admission_limit() {
        let control = AdmissionControl::new();
        let claims = (0..limits::MAX_CONNECTIONS_PER_SCOPE)
            .map(|_| control.claim_connection("t:b").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            control.claim_connection("t:b").err(),
            Some(AdmissionError::ScopeFull)
        );
        assert_eq!(
            control.stats().connections,
            limits::MAX_CONNECTIONS_PER_SCOPE
        );
        drop(claims);
        assert_eq!(control.stats().connections, 0);
    }

    #[test]
    fn one_identity_cannot_claim_unbounded_scopes() {
        let control = AdmissionControl::new();
        let claims = (0..limits::MAX_SCOPES_PER_IDENTITY)
            .map(|index| {
                control
                    .claim_replica(&format!("t:{index}"), "replica", index as u64 + 1)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            control.claim_replica("t:overflow", "replica", 99).err(),
            Some(AdmissionError::IdentityScopeLimit)
        );
        drop(claims);
        assert_eq!(control.stats().identities, 0);
    }

    #[test]
    fn scope_correlation_is_fixed_and_does_not_reveal_scope() {
        let correlation = scope_correlation("tenant-secret:board");
        assert_eq!(correlation.len(), 16);
        assert!(!correlation.contains("tenant"));
    }

    #[test]
    fn reconnect_storm_releases_all_aggregate_state() {
        let control = AdmissionControl::new();
        for index in 0..10_000 {
            let connection = control.claim_connection("t:storm").unwrap();
            let replica = control
                .claim_replica("t:storm", &format!("r{index}"), index + 1)
                .unwrap();
            drop(replica);
            drop(connection);
        }
        assert_eq!(control.stats().connections, 0);
        assert_eq!(control.stats().scopes, 0);
        assert_eq!(control.stats().identities, 0);
        assert_eq!(control.stats().claimed_replicas, 0);
    }
}
