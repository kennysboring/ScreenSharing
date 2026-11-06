//! Decides which frames are IDR: the first one, one every `gop` frames, and on request (a new
//! viewer, or a PLI from a browser that lost the picture) with a minimum interval between them.
//!
//! Times are `Duration`s on one monotonic clock chosen by the caller (the capture clock for the
//! screen source, time since start for the test source).

use std::time::Duration;

pub struct IdrScheduler {
    gop: u32,
    min_forced_interval: Duration,
    /// Frames emitted since the last IDR, the IDR included.
    since_idr: u32,
    last_idr: Option<Duration>,
    pending: bool,
}

impl IdrScheduler {
    /// `gop`: periodic IDR every `gop` frames. `min_forced_interval`: requested IDRs closer than
    /// this to the previous IDR wait until it has passed.
    pub fn new(gop: u32, min_forced_interval: Duration) -> Self {
        Self {
            gop: gop.max(1),
            min_forced_interval,
            since_idr: 0,
            last_idr: None,
            pending: false,
        }
    }

    /// Asks for an IDR as soon as the rate limit allows it.
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// Whether a request is waiting for the rate limit.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// When a pending request will be served (`None` without one): right away before the first
    /// IDR, else once `min_forced_interval` has passed since the last IDR.
    pub fn request_ready_at(&self) -> Option<Duration> {
        self.pending.then(|| {
            self.last_idr
                .map_or(Duration::ZERO, |last| last + self.min_forced_interval)
        })
    }

    /// Decides the frame about to be encoded at `now`. Any IDR, periodic or requested, restarts
    /// the GOP count and satisfies a pending request.
    pub fn next_is_idr(&mut self, now: Duration) -> bool {
        let idr = self.last_idr.is_none()
            || self.since_idr >= self.gop
            || self.request_ready_at().is_some_and(|ready| now >= ready);
        if idr {
            self.since_idr = 0;
            self.last_idr = Some(now);
            self.pending = false;
        }
        self.since_idr += 1;
        idr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(33);

    /// Runs `frames` frames 33 ms apart starting at `start`, requesting an IDR before the frames
    /// listed in `requests`, and returns the indexes of the IDR frames.
    fn run(
        scheduler: &mut IdrScheduler,
        start: Duration,
        frames: u32,
        requests: &[u32],
    ) -> Vec<u32> {
        (0..frames)
            .filter(|&i| {
                if requests.contains(&i) {
                    scheduler.request();
                }
                scheduler.next_is_idr(start + FRAME * i)
            })
            .collect()
    }

    #[test]
    fn test_first_frame_and_every_gop_are_idr() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        assert_eq!(run(&mut s, Duration::ZERO, 181, &[]), [0, 60, 120, 180]);
    }

    #[test]
    fn test_request_after_the_interval_is_served_on_the_next_frame() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // Frame 40 is 1.32 s after the IDR at frame 0
        assert_eq!(run(&mut s, Duration::ZERO, 100, &[40]), [0, 40]);
    }

    #[test]
    fn test_request_too_soon_waits_for_the_interval() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // Requested at frame 10 (0.33 s): served at frame 31, the first one at least 1 s later
        let idrs = run(&mut s, Duration::ZERO, 60, &[10]);
        assert_eq!(idrs, [0, 31]);
        assert!(!s.pending());
    }

    #[test]
    fn test_request_ready_at() {
        let second = Duration::from_secs(1);
        let mut s = IdrScheduler::new(60, second);
        assert_eq!(s.request_ready_at(), None, "no request");
        s.request();
        assert_eq!(
            s.request_ready_at(),
            Some(Duration::ZERO),
            "before the first IDR"
        );
        let t = Duration::from_secs(5);
        assert!(s.next_is_idr(t));
        assert_eq!(s.request_ready_at(), None, "served");
        s.request();
        assert_eq!(s.request_ready_at(), Some(t + second));
        assert!(!s.next_is_idr(t + second - Duration::from_millis(1)));
        assert!(s.next_is_idr(t + second));
    }

    #[test]
    fn test_forced_idr_restarts_the_gop() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // The periodic IDR moves from 60 to 40 + 60 = 100
        assert_eq!(run(&mut s, Duration::ZERO, 161, &[40]), [0, 40, 100, 160]);
    }

    #[test]
    fn test_periodic_idr_satisfies_a_pending_request() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(10));
        // The request at frame 50 cannot be served (10 s limit), but the periodic IDR at 60 does
        let idrs = run(&mut s, Duration::ZERO, 70, &[50]);
        assert_eq!(idrs, [0, 60]);
        assert!(!s.pending());
    }

    #[test]
    fn test_many_requests_give_at_most_one_idr_per_interval() {
        let mut s = IdrScheduler::new(1000, Duration::from_secs(1));
        let every_frame: Vec<u32> = (0..100).collect();
        // A request before every frame for 3.3 s: one IDR per second (30 frames of 33 ms)
        assert_eq!(
            run(&mut s, Duration::ZERO, 100, &every_frame),
            [0, 31, 62, 93]
        );
    }
}
