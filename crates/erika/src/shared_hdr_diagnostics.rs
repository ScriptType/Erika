//! Bounded diagnostic counters; these do not alter presentation or the clock.
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct Counts {
    submitted: u64,
    gpu_completed: u64,
    gpu_failed: u64,
    presented: u64,
    unpresented: u64,
    stale_callbacks: u64,
}

#[derive(Serialize)]
struct Interval {
    host_second: u64,
    counts: Counts,
}

#[derive(Serialize)]
struct Example {
    kind: &'static str,
    callback_host: f64,
    presented_host: Option<f64>,
    drawable_id: u64,
    generation: u64,
    frame_id: u64,
}

#[derive(Default, Serialize)]
pub(super) struct PresentationDiagnostics {
    totals: Counts,
    /// Last 120 host-second intervals, plus lifetime aggregate totals.
    intervals: VecDeque<Interval>,
    /// First eight positive and eight zero callbacks retain the initial transition.
    first_presented: Vec<Example>,
    first_unpresented: Vec<Example>,
    recent_callbacks: VecDeque<Example>,
}

impl PresentationDiagnostics {
    fn count(&mut self, host: f64, update: impl Fn(&mut Counts)) {
        update(&mut self.totals);
        let second = host.max(0.0).floor() as u64;
        // GPU callbacks can arrive from multiple threads out of order.
        if let Some(interval) = self.intervals.iter_mut().find(|v| v.host_second == second) {
            update(&mut interval.counts);
        } else {
            let mut interval = Interval { host_second: second, counts: Counts::default() };
            update(&mut interval.counts);
            self.intervals.push_back(interval);
            if self.intervals.len() > 120 { self.intervals.pop_front(); }
        }
    }

    pub(super) fn submitted(&mut self, host: f64) {
        self.count(host, |c| c.submitted += 1);
    }

    pub(super) fn gpu_complete(&mut self, host: f64, success: bool) {
        self.count(host, |c| if success { c.gpu_completed += 1 } else { c.gpu_failed += 1 });
    }

    pub(super) fn presented(&mut self, callback_host: f64, presented_host: f64,
                           drawable_id: u64, generation: u64, frame_id: u64, stale: bool) {
        let positive = presented_host.is_finite() && presented_host > 0.0;
        self.count(callback_host, |c| {
            if positive { c.presented += 1 } else { c.unpresented += 1 }
            if stale { c.stale_callbacks += 1 }
        });
        let sample = || Example {
            kind: if positive { "presented" } else { "unpresented" },
            callback_host, presented_host: positive.then_some(presented_host),
            drawable_id, generation, frame_id,
        };
        let first = if positive { &mut self.first_presented } else { &mut self.first_unpresented };
        if first.len() < 8 { first.push(sample()); }
        self.recent_callbacks.push_back(sample());
        if self.recent_callbacks.len() > 16 { self.recent_callbacks.pop_front(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_distinguish_skips_and_gpu_failure_with_bounded_retention() {
        let mut report = PresentationDiagnostics::default();
        for id in 0..1_000_u64 {
            report.submitted(id as f64);
            report.gpu_complete(id as f64, id != 500);
            report.presented(id as f64 + 0.1, if id % 2 == 0 { id as f64 + 0.05 } else { 0.0 },
                id, 2, id / 3, id == 10);
        }
        assert_eq!(report.totals.submitted, 1_000);
        assert_eq!(report.totals.gpu_completed, 999);
        assert_eq!(report.totals.gpu_failed, 1);
        assert_eq!(report.totals.presented, 500);
        assert_eq!(report.totals.unpresented, 500);
        assert_eq!(report.totals.stale_callbacks, 1);
        assert_eq!(report.intervals.len(), 120);
        assert_eq!(report.first_presented.len(), 8);
        assert_eq!(report.first_unpresented.len(), 8);
        assert_eq!(report.recent_callbacks.len(), 16);
    }
}
