//! Decides which frames are IDR: the first one, one every `gop` frames, and on request (a new
//! viewer, or a PLI from a browser that lost the picture) with a minimum interval between them.

use std::time::{Duration, Instant};

pub struct IdrScheduler {
    gop: u32,
    min_forced_interval: Duration,
    /// Frames emitted since the last IDR, the IDR included.
    since_idr: u32,
    last_idr: Option<Instant>,
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

    /// Decides the frame about to be encoded at `now`. Any IDR, periodic or requested, restarts
    /// the GOP count and satisfies a pending request.
    pub fn next_is_idr(&mut self, now: Instant) -> bool {
        let idr = match self.last_idr {
            None => true,
            Some(last) => {
                self.since_idr >= self.gop
                    || (self.pending
                        && now.saturating_duration_since(last) >= self.min_forced_interval)
            }
        };
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
        start: Instant,
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
        assert_eq!(run(&mut s, Instant::now(), 181, &[]), [0, 60, 120, 180]);
    }

    #[test]
    fn test_request_after_the_interval_is_served_on_the_next_frame() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // Frame 40 is 1.32 s after the IDR at frame 0
        assert_eq!(run(&mut s, Instant::now(), 100, &[40]), [0, 40]);
    }

    #[test]
    fn test_request_too_soon_waits_for_the_interval() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // Requested at frame 10 (0.33 s): served at frame 31, the first one at least 1 s later
        let idrs = run(&mut s, Instant::now(), 60, &[10]);
        assert_eq!(idrs, [0, 31]);
        assert!(!s.pending());
    }

    #[test]
    fn test_forced_idr_restarts_the_gop() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(1));
        // The periodic IDR moves from 60 to 40 + 60 = 100
        assert_eq!(run(&mut s, Instant::now(), 161, &[40]), [0, 40, 100, 160]);
    }

    #[test]
    fn test_periodic_idr_satisfies_a_pending_request() {
        let mut s = IdrScheduler::new(60, Duration::from_secs(10));
        // The request at frame 50 cannot be served (10 s limit), but the periodic IDR at 60 does
        let idrs = run(&mut s, Instant::now(), 70, &[50]);
        assert_eq!(idrs, [0, 60]);
        assert!(!s.pending());
    }

    #[test]
    fn test_many_requests_give_at_most_one_idr_per_interval() {
        let mut s = IdrScheduler::new(1000, Duration::from_secs(1));
        let every_frame: Vec<u32> = (0..100).collect();
        // A request before every frame for 3.3 s: one IDR per second (30 frames of 33 ms)
        assert_eq!(
            run(&mut s, Instant::now(), 100, &every_frame),
            [0, 31, 62, 93]
        );
    }
}
