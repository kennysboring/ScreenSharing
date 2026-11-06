//! RTP packets for one session, timestamped from the capture time of each frame.
//!
//! webrtc-rs's TrackLocalStaticSample stamps sample N with the running timestamp and only then
//! adds N's duration (rtc-rtp 0.21 `packetize`), so a duration moves the *next* frame. With a
//! variable frame rate that puts every gap on the wrong frame: after a static screen, the first
//! new frame would look early and the one after it late. Here the timestamp of each frame is
//! `base + (pts - first_pts) * 90 kHz`, so a frame always carries its own capture time.
//!
//! Payloading (STAP-A for SPS+PPS, FU-A for large NALs) is rtc's H264Payloader; this module only
//! adds the header: SSRC, payload type, sequence number, timestamp and the marker on the last
//! packet of each frame (RFC 6184 5.1), where TrackLocalStaticSample put it on every NAL.

use crate::video::EncodedFrame;
use rtc::rtp::{Header, Packet, codec::h264::H264Payloader, packetizer::Payloader};
use std::time::Duration;

/// Largest RTP packet, as webrtc-rs uses for its own packetizer.
pub const RTP_MTU: usize = 1200;
const RTP_HEADER_SIZE: usize = 12;
const VIDEO_CLOCK_RATE: u128 = 90_000;

/// Turns a session's frames into RTP packets.
///
/// TODO: when bandwidth control (rate adaptation) is added, these packets will need the
/// abs-send-time and transport-wide-cc header extensions: the browser builds its bandwidth
/// estimate (GCC) from them. TrackLocalStaticSample's packetizer added abs-send-time and these
/// packets carry none; whether webrtc-rs's TWCC interceptor adds transport-cc to packets sent
/// with write_rtp was not checked.
pub struct RtpStamper {
    payloader: H264Payloader,
    ssrc: u32,
    payload_type: u8,
    next_sequence: u16,
    timestamp_base: u32,
    first_pts: Option<Duration>,
}

impl RtpStamper {
    /// `first_sequence` and `timestamp_base` should be random (RFC 3550 5.1).
    pub fn new(ssrc: u32, payload_type: u8, first_sequence: u16, timestamp_base: u32) -> Self {
        Self {
            payloader: H264Payloader::default(),
            ssrc,
            payload_type,
            next_sequence: first_sequence,
            timestamp_base,
            first_pts: None,
        }
    }

    /// RTP timestamp of a frame captured at `pts`: 90 kHz ticks since the session's first frame.
    pub fn timestamp_for(&mut self, pts: Duration) -> u32 {
        let first = *self.first_pts.get_or_insert(pts);
        let ticks = pts.saturating_sub(first).as_nanos() * VIDEO_CLOCK_RATE / 1_000_000_000;
        // 32-bit RTP timestamps wrap (after about 13 hours at 90 kHz)
        self.timestamp_base.wrapping_add(ticks as u32)
    }

    /// Packets of one NAL. SPS and PPS give none: the payloader keeps them and sends them in a
    /// STAP-A in front of the next NAL, which shares their timestamp since they share the pts.
    pub fn packetize(&mut self, frame: &EncodedFrame) -> Result<Vec<Packet>, String> {
        let timestamp = self.timestamp_for(frame.pts);
        let payloads = self
            .payloader
            .payload(RTP_MTU - RTP_HEADER_SIZE, &frame.data)
            .map_err(|e| format!("H.264 payloader: {e}"))?;
        let count = payloads.len();
        Ok(payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| {
                let sequence_number = self.next_sequence;
                self.next_sequence = self.next_sequence.wrapping_add(1);
                Packet {
                    header: Header {
                        version: 2,
                        marker: frame.last_in_frame && i + 1 == count,
                        payload_type: self.payload_type,
                        sequence_number,
                        timestamp,
                        ssrc: self.ssrc,
                        ..Default::default()
                    },
                    payload,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    const MS: Duration = Duration::from_millis(1);

    fn nal(header: u8, len: usize, pts: Duration, last_in_frame: bool) -> EncodedFrame {
        let mut data = vec![0xab; len];
        data[0] = header;
        EncodedFrame {
            data: Bytes::from(data),
            pts,
            last_in_frame,
        }
    }

    fn p_frame(pts: Duration) -> EncodedFrame {
        nal(0x41, 100, pts, true)
    }

    #[test]
    fn test_timestamp_follows_the_capture_time() {
        let mut s = RtpStamper::new(1, 102, 0, 1000);
        assert_eq!(
            s.timestamp_for(5000 * MS),
            1000,
            "the first frame gets the base"
        );
        assert_eq!(s.timestamp_for(5033 * MS), 1000 + 2970); // 33 ms
        // Keepalive one second later: exactly 90000 ticks, nothing clamped
        assert_eq!(s.timestamp_for(6033 * MS), 1000 + 2970 + 90_000);
    }

    #[test]
    fn test_gap_after_a_static_screen_lands_on_the_late_frame() {
        let mut s = RtpStamper::new(1, 102, 0, 0);
        let ts: Vec<u32> = [0, 33, 2033, 2066]
            .map(|ms| s.packetize(&p_frame(ms * MS)).unwrap()[0].header.timestamp)
            .to_vec();
        // The 2 s pause shows between the 2nd and 3rd frames, not between the 3rd and 4th
        assert_eq!(ts, [0, 2970, 182_970, 185_940]);
    }

    #[test]
    fn test_headers_and_idr_share_one_timestamp_and_one_marker() {
        let mut s = RtpStamper::new(0x1234, 102, 7, 500);
        let pts = 40 * MS;
        let sps = s.packetize(&nal(0x67, 15, pts, false)).unwrap();
        let pps = s.packetize(&nal(0x68, 4, pts, false)).unwrap();
        let idr = s.packetize(&nal(0x65, 3000, pts, true)).unwrap();

        assert!(sps.is_empty() && pps.is_empty(), "held for the STAP-A");
        assert_eq!(
            idr[0].payload[0] & 0x1f,
            24,
            "STAP-A with SPS and PPS first"
        );
        assert!(idr.len() >= 4, "STAP-A plus the IDR split in FU-A");
        for (i, p) in idr.iter().enumerate() {
            assert_eq!(p.header.timestamp, 500);
            assert_eq!(p.header.ssrc, 0x1234);
            assert_eq!(p.header.payload_type, 102);
            assert_eq!(p.header.sequence_number, 7 + i as u16);
            assert_eq!(
                p.header.marker,
                i + 1 == idr.len(),
                "marker on the last packet only"
            );
            assert!(p.payload.len() <= RTP_MTU - RTP_HEADER_SIZE);
        }
    }

    #[test]
    fn test_marker_only_on_the_last_nal_of_a_frame() {
        let mut s = RtpStamper::new(1, 102, 0, 0);
        // A frame made of two slices: only the second ends the frame
        let first = s.packetize(&nal(0x41, 100, MS, false)).unwrap();
        let second = s.packetize(&nal(0x41, 100, MS, true)).unwrap();
        assert!(!first[0].header.marker);
        assert!(second[0].header.marker);
    }

    #[test]
    fn test_sequence_and_timestamp_wrap() {
        let mut s = RtpStamper::new(1, 102, u16::MAX, u32::MAX - 10);
        let a = s.packetize(&p_frame(Duration::ZERO)).unwrap();
        let b = s.packetize(&p_frame(MS)).unwrap();
        assert_eq!(a[0].header.sequence_number, u16::MAX);
        assert_eq!(b[0].header.sequence_number, 0);
        assert_eq!(b[0].header.timestamp, (u32::MAX - 10).wrapping_add(90));
    }
}
