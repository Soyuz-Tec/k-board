//! Fixed-cardinality operational telemetry primitives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kboard_core::document::ScopeId;

use crate::security::scope_correlation;

/// Stable histogram boundaries for external adapters. The in-process metrics
/// retain exact count/total/max; exporters may bucket into this fixed set.
pub const LATENCY_BUCKETS_MICROS: [u64; 12] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000,
];

#[derive(Default)]
pub struct LatencyMetric {
    count: AtomicU64,
    total_micros: AtomicU64,
    max_micros: AtomicU64,
}

impl LatencyMetric {
    pub fn observe_since(&self, started: Instant) {
        self.observe(started.elapsed());
    }

    pub fn observe(&self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            count: self.count.load(Ordering::Relaxed),
            total_micros: self.total_micros.load(Ordering::Relaxed),
            max_micros: self.max_micros.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct LatencySnapshot {
    pub count: u64,
    pub total_micros: u64,
    pub max_micros: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct CorrelationFields<'a> {
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

pub fn correlation_fields<'a>(
    scope: &ScopeId,
    replica: Option<&'a str>,
    batch: Option<&'a str>,
    sequence: Option<u64>,
) -> CorrelationFields<'a> {
    CorrelationFields {
        scope: scope_correlation(&scope.0),
        replica,
        batch,
        sequence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_is_structured_without_raw_scope_or_content() {
        let fields = correlation_fields(
            &ScopeId::new("secret-tenant:board"),
            Some("00112233445566778899aabbccddeeff"),
            Some("ffeeddccbbaa99887766554433221100"),
            Some(42),
        );
        let json = serde_json::to_string(&fields).unwrap();
        assert!(!json.contains("secret-tenant"));
        assert!(json.contains("00112233445566778899aabbccddeeff"));
        assert!(json.contains("\"sequence\":42"));
        assert_eq!(LATENCY_BUCKETS_MICROS.len(), 12);
    }
}
