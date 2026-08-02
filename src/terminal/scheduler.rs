use std::time::{Duration, Instant};

use super::FrameRevision;

#[derive(Debug)]
pub struct LatestFrameScheduler<T> {
    pending: Option<(FrameRevision, T)>,
    newest_seen: FrameRevision,
    last_emitted: Option<Instant>,
    min_interval: Duration,
}

impl<T> LatestFrameScheduler<T> {
    #[must_use]
    pub fn new(max_fps: u32) -> Self {
        let max_fps = max_fps.max(1);
        Self {
            pending: None,
            newest_seen: FrameRevision::ZERO,
            last_emitted: None,
            min_interval: Duration::from_secs_f64(1.0 / f64::from(max_fps)),
        }
    }

    pub fn submit(&mut self, revision: FrameRevision, value: T) -> bool {
        if revision <= self.newest_seen {
            return false;
        }
        self.newest_seen = revision;
        self.pending = Some((revision, value));
        true
    }

    pub fn take_ready(&mut self, now: Instant) -> Option<(FrameRevision, T)> {
        if let Some(last_emitted) = self.last_emitted
            && now.saturating_duration_since(last_emitted) < self.min_interval
        {
            return None;
        }
        let pending = self.pending.take()?;
        self.last_emitted = Some(now);
        Some(pending)
    }

    #[must_use]
    pub const fn pending_len(&self) -> usize {
        if self.pending.is_some() { 1 } else { 0 }
    }

    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_frame_replaces_pending_work() {
        let start = Instant::now();
        let mut scheduler = LatestFrameScheduler::new(60);
        assert!(scheduler.submit(FrameRevision::new(1), "old"));
        assert!(scheduler.submit(FrameRevision::new(2), "new"));
        assert_eq!(scheduler.pending_len(), 1);
        assert_eq!(
            scheduler.take_ready(start),
            Some((FrameRevision::new(2), "new"))
        );
        assert!(
            scheduler
                .take_ready(start + Duration::from_millis(1))
                .is_none()
        );
        assert!(
            scheduler
                .take_ready(start + Duration::from_millis(17))
                .is_none()
        );
        assert!(scheduler.is_idle());
    }

    #[test]
    fn stale_revision_cannot_reopen_the_queue() {
        let mut scheduler = LatestFrameScheduler::new(120);
        assert!(scheduler.submit(FrameRevision::new(1), "one"));
        let now = Instant::now();
        assert_eq!(
            scheduler.take_ready(now),
            Some((FrameRevision::new(1), "one"))
        );
        assert!(!scheduler.submit(FrameRevision::new(1), "stale"));
        assert!(!scheduler.submit(FrameRevision::new(0), "older"));
        assert!(scheduler.is_idle());
    }
}
