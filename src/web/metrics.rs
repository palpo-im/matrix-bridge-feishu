use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use salvo::prelude::*;

#[derive(Default)]
pub struct BridgeMetrics {
    inbound_events_total: AtomicU64,
    outbound_calls_total: AtomicU64,
    outbound_failures_total: AtomicU64,
    cache_hits_total: AtomicU64,
    cache_misses_total: AtomicU64,
    queue_depth: AtomicU64,
    queue_depth_max: AtomicU64,
    inbound_by_event: Mutex<HashMap<String, u64>>,
    outbound_by_api: Mutex<HashMap<String, u64>>,
    outbound_failures_by_api_code: Mutex<HashMap<String, u64>>,
    cache_hits_by_name: Mutex<HashMap<String, u64>>,
    cache_misses_by_name: Mutex<HashMap<String, u64>>,
    processing_stats: Mutex<HashMap<String, ProcessingStats>>,
}

#[derive(Default, Clone, Copy)]
struct ProcessingStats {
    count: u64,
    sum_ms: u64,
}

static GLOBAL_METRICS: OnceLock<BridgeMetrics> = OnceLock::new();

pub fn global_metrics() -> &'static BridgeMetrics {
    GLOBAL_METRICS.get_or_init(BridgeMetrics::default)
}

impl BridgeMetrics {
    pub fn record_inbound_event(&self, event_type: &str) {
        self.inbound_events_total.fetch_add(1, Ordering::Relaxed);
        increment_map(&self.inbound_by_event, event_type.to_string());
    }

    pub fn record_outbound_call(&self, api: &str) {
        self.outbound_calls_total.fetch_add(1, Ordering::Relaxed);
        increment_map(&self.outbound_by_api, api.to_string());
    }

    pub fn record_outbound_failure(&self, api: &str, code: &str) {
        self.outbound_failures_total.fetch_add(1, Ordering::Relaxed);
        let key = format!("{}|{}", api, code);
        increment_map(&self.outbound_failures_by_api_code, key);
    }

    pub fn record_cache_hit(&self, cache_name: &str) {
        self.cache_hits_total.fetch_add(1, Ordering::Relaxed);
        increment_map(&self.cache_hits_by_name, cache_name.to_string());
    }

    pub fn record_cache_miss(&self, cache_name: &str) {
        self.cache_misses_total.fetch_add(1, Ordering::Relaxed);
        increment_map(&self.cache_misses_by_name, cache_name.to_string());
    }

    pub fn record_processing_duration(&self, stage: &str, duration: Duration) {
        let mut guard = self
            .processing_stats
            .lock()
            .expect("metrics mutex poisoned");
        let entry = guard.entry(stage.to_string()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.sum_ms = entry
            .sum_ms
            .saturating_add(duration.as_millis().min(u64::MAX as u128) as u64);
    }

    pub fn begin_queue_task(&self) -> QueueDepthGuard {
        let current = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        let mut max_seen = self.queue_depth_max.load(Ordering::Relaxed);
        while current > max_seen {
            match self.queue_depth_max.compare_exchange(
                max_seen,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => max_seen = actual,
            }
        }
        QueueDepthGuard {}
    }

    fn end_queue_task(&self) {
        let _ = self
            .queue_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(1))
            });
    }

    pub fn render_prometheus(&self) -> String {
        let mut body = String::new();

        body.push_str("# HELP bridge_inbound_events_total Total inbound events\n");
        body.push_str("# TYPE bridge_inbound_events_total counter\n");
        body.push_str(&format!(
            "bridge_inbound_events_total {}\n",
            self.inbound_events_total.load(Ordering::Relaxed)
        ));

        for (event_type, count) in sorted_pairs(&self.inbound_by_event) {
            body.push_str(&format!(
                "bridge_inbound_events_total_by_type{{event_type=\"{}\"}} {}\n",
                escape_label(&event_type),
                count
            ));
        }

        body.push_str("# HELP bridge_outbound_calls_total Total outbound API calls\n");
        body.push_str("# TYPE bridge_outbound_calls_total counter\n");
        body.push_str(&format!(
            "bridge_outbound_calls_total {}\n",
            self.outbound_calls_total.load(Ordering::Relaxed)
        ));

        for (api, count) in sorted_pairs(&self.outbound_by_api) {
            body.push_str(&format!(
                "bridge_outbound_calls_total_by_api{{api=\"{}\"}} {}\n",
                escape_label(&api),
                count
            ));
        }

        body.push_str("# HELP bridge_outbound_failures_total Total outbound API failures\n");
        body.push_str("# TYPE bridge_outbound_failures_total counter\n");
        body.push_str(&format!(
            "bridge_outbound_failures_total {}\n",
            self.outbound_failures_total.load(Ordering::Relaxed)
        ));

        for (api_code, count) in sorted_pairs(&self.outbound_failures_by_api_code) {
            let mut parts = api_code.splitn(2, '|');
            let api = parts.next().unwrap_or("unknown");
            let code = parts.next().unwrap_or("unknown");
            body.push_str(&format!(
                "bridge_outbound_failures_total_by_api_code{{api=\"{}\",code=\"{}\"}} {}\n",
                escape_label(api),
                escape_label(code),
                count
            ));
        }

        body.push_str("# HELP bridge_cache_hits_total Total cache hits\n");
        body.push_str("# TYPE bridge_cache_hits_total counter\n");
        body.push_str(&format!(
            "bridge_cache_hits_total {}\n",
            self.cache_hits_total.load(Ordering::Relaxed)
        ));
        body.push_str("# HELP bridge_cache_misses_total Total cache misses\n");
        body.push_str("# TYPE bridge_cache_misses_total counter\n");
        body.push_str(&format!(
            "bridge_cache_misses_total {}\n",
            self.cache_misses_total.load(Ordering::Relaxed)
        ));

        let hits_by_cache = sorted_pairs(&self.cache_hits_by_name);
        let misses_by_cache = sorted_pairs(&self.cache_misses_by_name);
        let mut all_cache_names: Vec<String> = hits_by_cache
            .iter()
            .map(|(name, _)| name.clone())
            .chain(misses_by_cache.iter().map(|(name, _)| name.clone()))
            .collect();
        all_cache_names.sort();
        all_cache_names.dedup();

        body.push_str("# HELP bridge_cache_requests_total Cache requests by result\n");
        body.push_str("# TYPE bridge_cache_requests_total counter\n");
        body.push_str("# HELP bridge_cache_hit_ratio Cache hit ratio by cache name\n");
        body.push_str("# TYPE bridge_cache_hit_ratio gauge\n");

        for cache_name in all_cache_names {
            let hits = hits_by_cache
                .iter()
                .find(|(name, _)| name == &cache_name)
                .map(|(_, value)| *value)
                .unwrap_or(0);
            let misses = misses_by_cache
                .iter()
                .find(|(name, _)| name == &cache_name)
                .map(|(_, value)| *value)
                .unwrap_or(0);
            let total = hits + misses;
            let ratio = if total == 0 {
                0.0
            } else {
                hits as f64 / total as f64
            };

            body.push_str(&format!(
                "bridge_cache_requests_total{{cache=\"{}\",result=\"hit\"}} {}\n",
                escape_label(&cache_name),
                hits
            ));
            body.push_str(&format!(
                "bridge_cache_requests_total{{cache=\"{}\",result=\"miss\"}} {}\n",
                escape_label(&cache_name),
                misses
            ));
            body.push_str(&format!(
                "bridge_cache_hit_ratio{{cache=\"{}\"}} {:.6}\n",
                escape_label(&cache_name),
                ratio
            ));
        }

        body.push_str("# HELP bridge_queue_depth Current webhook queue depth\n");
        body.push_str("# TYPE bridge_queue_depth gauge\n");
        body.push_str(&format!(
            "bridge_queue_depth {}\n",
            self.queue_depth.load(Ordering::Relaxed)
        ));
        body.push_str(&format!(
            "bridge_queue_depth_max {}\n",
            self.queue_depth_max.load(Ordering::Relaxed)
        ));

        body.push_str("# HELP bridge_processing_duration_ms_sum Total processing duration in ms\n");
        body.push_str("# TYPE bridge_processing_duration_ms_sum counter\n");
        for (stage, stats) in sorted_processing(&self.processing_stats) {
            body.push_str(&format!(
                "bridge_processing_duration_ms_sum{{stage=\"{}\"}} {}\n",
                escape_label(&stage),
                stats.sum_ms
            ));
            body.push_str(&format!(
                "bridge_processing_duration_ms_count{{stage=\"{}\"}} {}\n",
                escape_label(&stage),
                stats.count
            ));
        }

        body
    }
}

pub struct QueueDepthGuard {}

impl Drop for QueueDepthGuard {
    fn drop(&mut self) {
        global_metrics().end_queue_task();
    }
}

pub struct ScopedTimer {
    stage: String,
    started_at: Instant,
}

impl ScopedTimer {
    pub fn new(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            started_at: Instant::now(),
        }
    }
}

impl Drop for ScopedTimer {
    fn drop(&mut self) {
        global_metrics().record_processing_duration(&self.stage, self.started_at.elapsed());
    }
}

#[handler]
pub async fn metrics_endpoint(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render(global_metrics().render_prometheus());
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn increment_map(map: &Mutex<HashMap<String, u64>>, key: String) {
    let mut guard = map.lock().expect("metrics mutex poisoned");
    let counter = guard.entry(key).or_insert(0);
    *counter = counter.saturating_add(1);
}

fn sorted_pairs(map: &Mutex<HashMap<String, u64>>) -> Vec<(String, u64)> {
    let guard = map.lock().expect("metrics mutex poisoned");
    let mut values: Vec<(String, u64)> = guard.iter().map(|(k, v)| (k.clone(), *v)).collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

fn sorted_processing(
    map: &Mutex<HashMap<String, ProcessingStats>>,
) -> Vec<(String, ProcessingStats)> {
    let guard = map.lock().expect("metrics mutex poisoned");
    let mut values: Vec<(String, ProcessingStats)> =
        guard.iter().map(|(k, v)| (k.clone(), *v)).collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}
