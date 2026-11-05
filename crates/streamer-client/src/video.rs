//! Sends a pre-encoded H.264 file through a video track, one frame every 33 ms.

use rtc::{
    media::{Sample, io::h26x_reader::H26xSampleReader},
    rtp_transceiver::PayloadType,
};
use std::{
    fs::File,
    io::BufReader,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc::Receiver, time::interval};
use webrtc::media_stream::{Track, track_local::static_sample::TrackLocalStaticSample};

#[derive(Default)]
pub struct KeyframeGate {
    open: bool,
}

impl KeyframeGate {
    pub fn should_send(&mut self, nal: Option<u8>) -> bool {
        if nal.is_none() {
            return false;
        }
        if self.open || nal == Some(7) {
            self.open = true;
            return true;
        }
        false
    }

    pub fn wait_for_keyframe(&mut self) {
        self.open = false
    }
}

const TEST_VIDEO_PATH: &str = "test-assets/test.h264";
const H26X_FRAME_DURATION: Duration = Duration::from_millis(33);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts sending the test video in the background once the connection is up, giving up if it
/// does not connect within `CONNECT_TIMEOUT`.
pub fn start_test_video(
    track: Arc<TrackLocalStaticSample>,
    payload_type: PayloadType,
    mut connected_rx: Receiver<()>,
) {
    tokio::spawn(async move {
        match tokio::time::timeout(CONNECT_TIMEOUT, connected_rx.recv()).await {
            Ok(Some(())) => {
                if stream_video(TEST_VIDEO_PATH, track, payload_type)
                    .await
                    .is_err()
                {
                    println!("Error streaming video");
                }
            }
            Ok(None) => println!("Connection closed before it was established"),
            Err(_) => println!("Connection timed out while waiting to connect"),
        }
    });
}

async fn stream_video(
    path: &str,
    track: Arc<TrackLocalStaticSample>,
    payload_type: PayloadType,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut video_reader = H26xSampleReader::new(reader, 1_048_576, false);

    let ssrc = *track.ssrcs().await.first().ok_or("track has no SSRC")?;

    let mut ticker = interval(H26X_FRAME_DURATION);
    loop {
        let sample = match video_reader.next_sample() {
            Ok(sample) => sample,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data: sample.data,
                duration: if sample.timed {
                    H26X_FRAME_DURATION
                } else {
                    Duration::ZERO
                },
                ..Sample::new(Instant::now())
            })
            .await?;

        if sample.timed {
            let _ = ticker.tick().await;
        }
    }

    Ok(())
}

/// Returns the H.264 NAL unit type (the low 5 bits of the first byte), or `None` if `data` is
/// empty.
fn nal_type(data: &[u8]) -> Option<u8> {
    data.first().map(|byte| byte & 0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::make_offer;

    fn temp_file_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "streamer-client-{}-{}-{name}",
            std::process::id(),
            rand::random::<u32>()
        ))
    }

    #[tokio::test]
    async fn test_stream_video_missing_file_fails() {
        let (_sdp, session) = make_offer().await.expect("should create offer");
        let path = temp_file_path("missing.h264");

        let result = stream_video(path.to_str().unwrap(), session.track.clone(), 102).await;
        assert!(result.is_err());

        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_stream_video_empty_file_ends_ok() {
        let (_sdp, session) = make_offer().await.expect("should create offer");
        let path = temp_file_path("empty.h264");
        std::fs::write(&path, []).expect("should create empty file");

        let result = stream_video(path.to_str().unwrap(), session.track.clone(), 102).await;
        let _ = std::fs::remove_file(&path);
        assert!(result.is_ok());

        let _ = session.pc.close().await;
    }

    #[test]
    fn test_nal_type() {
        // First bytes of real samples from test-assets/test.h264
        assert_eq!(nal_type(&[0x67, 0x42, 0xc0, 0x1f, 0xd9, 0x00]), Some(7)); // SPS
        assert_eq!(nal_type(&[0x68, 0xcb, 0x83, 0xcb, 0x20]), Some(8)); // PPS
        assert_eq!(nal_type(&[0x65, 0x88, 0x84, 0x0b, 0xf2, 0x62]), Some(5)); // IDR
        assert_eq!(nal_type(&[0x41, 0x9a, 0x38, 0x17, 0xe0, 0x61]), Some(1)); // P-frame
        assert_eq!(nal_type(&[0x25]), Some(5)); // IDR with a different nal_ref_idc
        assert_eq!(nal_type(&[0x06]), Some(6)); // SEI
        assert_eq!(nal_type(&[]), None);
    }

    /// Feeds each NAL type through the gate and returns what it decided for each one.
    fn feed(gate: &mut KeyframeGate, nals: &[u8]) -> Vec<bool> {
        nals.iter()
            .map(|&nal| gate.should_send(Some(nal)))
            .collect()
    }

    #[test]
    fn test_keyframe_gate_opens_on_sps() {
        let mut gate = KeyframeGate::default();
        assert_eq!(
            feed(&mut gate, &[1, 1, 7, 8, 5, 1]),
            [false, false, true, true, true, true]
        );
    }

    #[test]
    fn test_keyframe_gate_idr_without_sps_stays_closed() {
        // Without the SPS/PPS headers the browser cannot decode the IDR
        let mut gate = KeyframeGate::default();
        assert_eq!(feed(&mut gate, &[5, 5]), [false, false]);
    }

    #[test]
    fn test_keyframe_gate_wait_for_keyframe_closes_again() {
        let mut gate = KeyframeGate::default();
        assert_eq!(feed(&mut gate, &[7, 1]), [true, true]);

        gate.wait_for_keyframe();

        assert_eq!(feed(&mut gate, &[1, 7, 1]), [false, true, true]);
    }

    #[test]
    fn test_keyframe_gate_idr_after_close_needs_sps() {
        // Even right after an IDR, once closed only an SPS opens the gate again
        let mut gate = KeyframeGate::default();
        assert_eq!(feed(&mut gate, &[7, 8, 5]), [true, true, true]);

        gate.wait_for_keyframe();

        assert_eq!(feed(&mut gate, &[5, 1, 7]), [false, false, true]);
    }

    #[test]
    fn test_keyframe_gate_none_is_never_sent() {
        let mut gate = KeyframeGate::default();
        assert!(!gate.should_send(None), "closed gate");

        assert!(gate.should_send(Some(7)));
        assert!(!gate.should_send(None), "open gate");
        // An empty sample does not close the gate
        assert!(gate.should_send(Some(1)));
    }

    #[test]
    fn test_keyframe_gate_starts_closed() {
        let mut gate = KeyframeGate::default();
        assert!(!gate.should_send(Some(1)));
    }
}
