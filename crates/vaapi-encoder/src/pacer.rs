//! Decides when the screen source encodes and which pts each encoded frame gets.
//!
//! A screen capture only delivers a frame when something changed, so the encoder input surface
//! keeps the last frame and is encoded again when:
//! - a new frame arrived (pts = its capture time), at most `max_fps` times per second; a frame
//!   that comes too soon waits, and a newer one replaces it;
//! - an IDR was requested (new viewer, PLI) and the [`IdrScheduler`] accepts it;
//! - nothing was encoded for `keepalive` (1 s), so the browser keeps receiving frames.
//!
//! A re-encoded frame shows the screen *now*, so it gets pts = now, never the pts of the last
//! capture: with the old pts the RTP timestamp would go back or stand still and the browser
//! would drop or misplace the frame. pts are also strictly increasing, by at least one 90 kHz
//! RTP tick, so two frames never share an RTP timestamp.
//!
//! Pure logic: `now`, the capture pts and the scheduler use one monotonic clock chosen by the
//! caller (for PipeWire, CLOCK_MONOTONIC: `spa_meta_header.pts` is on it).

use crate::schedule::IdrScheduler;
use std::time::Duration;

/// One 90 kHz RTP tick (11 111.1 ns), rounded up: pts this far apart give different RTP
/// timestamps.
pub const MIN_PTS_STEP: Duration = Duration::from_nanos(11_112);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NewFrame,
    Keyframe,
    Keepalive,
}

/// Encode the input surface now, with this pts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encode {
    pub pts: Duration,
    pub idr: bool,
    pub reason: Reason,
}

pub struct FramePacer {
    min_interval: Duration,
    keepalive: Duration,
    idrs: IdrScheduler,
    /// Capture pts of a frame in the input surface that was not encoded yet
    pending_frame: Option<Duration>,
    last_encode: Option<Duration>,
    last_pts: Option<Duration>,
}

impl FramePacer {
    pub fn new(max_fps: u32, keepalive: Duration, idrs: IdrScheduler) -> Self {
        Self {
            min_interval: Duration::from_secs(1) / max_fps.max(1),
            keepalive,
            idrs,
            pending_frame: None,
            last_encode: None,
            last_pts: None,
        }
    }

    /// A new frame, captured at `capture_pts`, is now in the input surface. It replaces a frame
    /// still waiting for the fps cap: that one was overwritten.
    pub fn frame_arrived(&mut self, capture_pts: Duration) {
        self.pending_frame = Some(capture_pts);
    }

    /// A session asked for an IDR.
    pub fn request_keyframe(&mut self) {
        self.idrs.request();
    }

    /// What to do at `now`: `Some` means encode the input surface now. Call it after every
    /// event and when the timer set from [`Self::next_wakeup`] fires.
    pub fn poll(&mut self, now: Duration) -> Option<Encode> {
        let allowed = self
            .last_encode
            .is_none_or(|last| now >= last + self.min_interval);
        if !allowed {
            return None;
        }
        let (pts, reason) = if let Some(capture_pts) = self.pending_frame.take() {
            (capture_pts, Reason::NewFrame)
        } else {
            // Nothing to re-encode before the first frame (that one will be an IDR anyway)
            let last = self.last_encode?;
            if self
                .idrs
                .request_ready_at()
                .is_some_and(|ready| now >= ready)
            {
                (now, Reason::Keyframe)
            } else if now >= last + self.keepalive {
                (now, Reason::Keepalive)
            } else {
                return None;
            }
        };
        // A frame captured before the last keepalive, or a capture clock slightly ahead of
        // `now`, must not make the pts go back or repeat
        let pts = match self.last_pts {
            Some(last_pts) if pts < last_pts + MIN_PTS_STEP => last_pts + MIN_PTS_STEP,
            _ => pts,
        };
        self.last_encode = Some(now);
        self.last_pts = Some(pts);
        Some(Encode {
            pts,
            idr: self.idrs.next_is_idr(now),
            reason,
        })
    }

    /// When [`Self::poll`] can next return `Some` if no event comes first; `None` while waiting
    /// for the first frame.
    pub fn next_wakeup(&self) -> Option<Duration> {
        let Some(last) = self.last_encode else {
            return self.pending_frame.map(|_| Duration::ZERO);
        };
        let allowed = last + self.min_interval;
        if self.pending_frame.is_some() {
            return Some(allowed);
        }
        let mut deadline = last + self.keepalive;
        if let Some(ready) = self.idrs.request_ready_at() {
            deadline = deadline.min(ready);
        }
        Some(deadline.max(allowed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const SECOND: Duration = Duration::from_secs(1);

    /// 60 fps cap, 1 s keepalive, periodic IDR every 120 frames, forced IDRs 1 s apart.
    fn pacer() -> FramePacer {
        FramePacer::new(60, SECOND, IdrScheduler::new(120, SECOND))
    }

    fn rtp_ticks(pts: Duration) -> u128 {
        pts.as_nanos() * 90_000 / 1_000_000_000
    }

    #[test]
    fn test_nothing_before_the_first_frame() {
        let mut p = pacer();
        p.request_keyframe();
        assert_eq!(p.poll(5 * SECOND), None);
        assert_eq!(p.next_wakeup(), None);

        p.frame_arrived(5 * SECOND);
        assert_eq!(p.next_wakeup(), Some(Duration::ZERO));
        let first = p.poll(5 * SECOND + MS).unwrap();
        assert_eq!(first.pts, 5 * SECOND, "the capture time, not the poll time");
        assert!(first.idr);
        assert_eq!(first.reason, Reason::NewFrame);
    }

    #[test]
    fn test_keepalive_on_a_static_screen_gets_the_current_time() {
        let mut p = pacer();
        let capture = 10 * SECOND;
        p.frame_arrived(capture);
        p.poll(capture + 2 * MS).unwrap();

        assert_eq!(p.next_wakeup(), Some(capture + 2 * MS + SECOND));
        assert_eq!(p.poll(capture + 500 * MS), None);
        let now = capture + 2 * MS + SECOND;
        let keepalive = p.poll(now).unwrap();
        assert_eq!(keepalive.reason, Reason::Keepalive);
        assert_eq!(keepalive.pts, now, "now, not the last capture pts");
        assert!(!keepalive.idr);

        let later = p.poll(now + SECOND).unwrap();
        assert_eq!(later.pts, now + SECOND);
    }

    #[test]
    fn test_forced_idr_on_a_static_screen_gets_the_current_time() {
        let mut p = pacer();
        let capture = 10 * SECOND;
        p.frame_arrived(capture);
        assert!(p.poll(capture).unwrap().idr);

        // A viewer joins 3 s later; the screen has not changed since `capture`
        let now = capture + 3 * SECOND;
        p.request_keyframe();
        let idr = p.poll(now).unwrap();
        assert_eq!(idr.reason, Reason::Keyframe);
        assert!(idr.idr);
        assert_eq!(idr.pts, now);
    }

    #[test]
    fn test_forced_idr_too_soon_waits_for_the_scheduler() {
        let mut p = pacer();
        p.frame_arrived(Duration::ZERO);
        assert!(p.poll(Duration::ZERO).unwrap().idr);

        // Requested 300 ms after the first IDR: served 1 s after it, with that pts
        p.request_keyframe();
        assert_eq!(p.poll(300 * MS), None);
        assert_eq!(p.next_wakeup(), Some(SECOND));
        let idr = p.poll(SECOND).unwrap();
        assert_eq!(
            (idr.reason, idr.idr, idr.pts),
            (Reason::Keyframe, true, SECOND)
        );
    }

    #[test]
    fn test_max_fps_defers_a_frame_and_keeps_the_newest() {
        let mut p = pacer();
        p.frame_arrived(Duration::ZERO);
        p.poll(Duration::ZERO).unwrap();

        // Two frames within one 60 fps interval (16.67 ms): the second overwrote the first
        p.frame_arrived(5 * MS);
        assert_eq!(p.poll(5 * MS), None);
        p.frame_arrived(10 * MS);
        let allowed = SECOND / 60;
        assert_eq!(p.next_wakeup(), Some(allowed));
        assert_eq!(p.poll(allowed - Duration::from_nanos(1)), None);
        let frame = p.poll(allowed).unwrap();
        assert_eq!(frame.reason, Reason::NewFrame);
        assert_eq!(frame.pts, 10 * MS, "the newest capture keeps its own time");
        assert_eq!(p.poll(allowed + SECOND / 2), None, "nothing new to send");
    }

    #[test]
    fn test_late_frame_older_than_the_keepalive_is_moved_after_it() {
        let mut p = pacer();
        p.frame_arrived(Duration::ZERO);
        p.poll(Duration::ZERO).unwrap();
        let keepalive = p.poll(SECOND).unwrap();
        assert_eq!(keepalive.pts, SECOND);

        // Captured just before the keepalive but delivered after it
        p.frame_arrived(SECOND - MS);
        let frame = p.poll(SECOND + 20 * MS).unwrap();
        assert_eq!(frame.pts, SECOND + MIN_PTS_STEP);
        assert!(rtp_ticks(frame.pts) > rtp_ticks(keepalive.pts));
    }

    #[test]
    fn test_pts_strictly_increase_through_keepalives_idrs_and_frames() {
        let mut p = pacer();
        let mut out = Vec::new();
        // 1 ms steps for 6 s: a burst of frames, a static screen with keepalives, a viewer
        // joining during it, a frame stamped slightly in the future, then a frame captured
        // before the keepalive that went out ahead of it
        for ms in 0..6000u32 {
            let now = ms * MS;
            match ms {
                0..=99 => p.frame_arrived(now),
                1500 => p.request_keyframe(),
                3000 => p.frame_arrived(now + 3 * MS),
                5005 => p.frame_arrived(now - 10 * MS),
                _ => {}
            }
            if let Some(encode) = p.poll(now) {
                out.push(encode);
            }
        }

        for pair in out.windows(2) {
            assert!(pair[1].pts > pair[0].pts, "{pair:?}");
            assert!(rtp_ticks(pair[1].pts) > rtp_ticks(pair[0].pts), "{pair:?}");
        }
        let static_screen: Vec<_> = out
            .iter()
            .filter(|e| e.reason != Reason::NewFrame)
            .map(|e| (e.reason, e.idr, e.pts.as_millis()))
            .collect();
        assert_eq!(
            static_screen,
            [
                // 1 s after the last frame of the burst was sent (at 102 ms, the 60 fps cap)
                (Reason::Keepalive, false, 1102),
                (Reason::Keyframe, true, 1500),
                (Reason::Keepalive, false, 2500),
                // After the frame sent at 3000 ms with pts 3003 ms
                (Reason::Keepalive, false, 4000),
                (Reason::Keepalive, false, 5000),
            ],
            "re-encoded frames carry the time they were sent"
        );
        let late = out.last().unwrap();
        assert_eq!(late.reason, Reason::NewFrame);
        assert_eq!(
            late.pts,
            5 * SECOND + MIN_PTS_STEP,
            "moved after the keepalive"
        );
    }
}
