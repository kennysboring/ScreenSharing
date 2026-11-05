use bytes::Bytes;
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
use tokio::{
    sync::{broadcast, mpsc::Receiver},
    time::{Interval, interval},
};
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

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub timed: bool,
}

pub const TEST_VIDEO_PATH: &str = "test-assets/test.h264";
const H26X_FRAME_DURATION: Duration = Duration::from_millis(33);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Once the connection is up, subscribes this session to the shared frame source and sends the
/// frames to its track, starting at the next keyframe. Gives up if it does not connect within
/// `CONNECT_TIMEOUT`.
pub fn start_session_video(
    track: Arc<TrackLocalStaticSample>,
    payload_type: PayloadType,
    mut connected_rx: Receiver<()>,
    frames_tx: broadcast::Sender<EncodedFrame>,
) {
    tokio::spawn(async move {
        match tokio::time::timeout(CONNECT_TIMEOUT, connected_rx.recv()).await {
            Ok(Some(())) => {
                // Subscribe only now, so no frames pile up while ICE is negotiating
                let frames_rx = frames_tx.subscribe();
                // Holding a sender here would keep the channel from ever closing
                drop(frames_tx);

                let Some(&ssrc) = track.ssrcs().await.first() else {
                    println!("Track has no SSRC");
                    return;
                };
                // Each call owns a handle to the track, so the future does not borrow the
                // closure and the task stays Send
                let write = |data: Bytes, duration: Duration| {
                    let track = Arc::clone(&track);
                    async move {
                        track
                            .sample_writer(ssrc, payload_type)
                            .write_sample(&Sample {
                                data,
                                duration,
                                ..Sample::new(Instant::now())
                            })
                            .await
                    }
                };
                // The error is not printed: a failed write carries the whole RTP packet
                if forward_frames(frames_rx, write).await.is_err() {
                    println!("Error sending video");
                }
            }
            Ok(None) => println!("Connection closed before it was established"),
            Err(_) => println!("Connection timed out while waiting to connect"),
        }
    });
}

/// Passes each frame from the source to `sink`, holding them back until an SPS. Ends when the
/// source closes, or with the sink's error (in production, a failed write: the viewer left).
///
/// The decisions live here and the I/O in `sink`, so they can be tested without WebRTC.
///
/// TODO(encoder): a session that joins mid-stream waits for the next SPS, up to one GOP (about
/// 2 s with the test file). A pre-encoded file cannot do better. With a real encoder, ask it for
/// a keyframe when a session subscribes or a PLI arrives from the browser.
async fn forward_frames<E, F, Fut>(
    mut frames_rx: broadcast::Receiver<EncodedFrame>,
    mut sink: F,
) -> Result<(), E>
where
    F: FnMut(Bytes, Duration) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let mut gate = KeyframeGate::default();

    loop {
        match frames_rx.recv().await {
            Ok(frame) => {
                if !gate.should_send(nal_type(&frame.data)) {
                    continue;
                }
                // SPS, PPS and IDR are one frame: only the timed one moves the clock
                let duration = if frame.timed {
                    H26X_FRAME_DURATION
                } else {
                    Duration::ZERO
                };
                sink(frame.data, duration).await?;
            }
            // Frames were dropped, so the next P-frames lack their reference: wait for an SPS
            Err(broadcast::error::RecvError::Lagged(_)) => gate.wait_for_keyframe(),
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

/// Returns the H.264 NAL unit type (the low 5 bits of the first byte), or `None` if `data` is
/// empty.
fn nal_type(data: &[u8]) -> Option<u8> {
    data.first().map(|byte| byte & 0x1f)
}

/// Sends the test video to `tx` in the background, starting over each time the file ends.
/// Stops if the file cannot be read or has no frames.
pub fn start_test_source(tx: broadcast::Sender<EncodedFrame>, path: String) {
    tokio::spawn(async move {
        // One ticker for every pass, so the pace carries on across the restart
        let mut ticker = interval(H26X_FRAME_DURATION);
        loop {
            match send_file_once(&tx, &path, &mut ticker).await {
                // Without this check an empty file would restart forever without waiting
                Ok(0) => {
                    eprintln!("Test video has no frames: {path}");
                    return;
                }
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("Error reading the test video: {e}");
                    return;
                }
            }
        }
    });
}

async fn send_file_once(
    tx: &broadcast::Sender<EncodedFrame>,
    path: &str,
    ticker: &mut Interval,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let file = File::open(path)?;
    let mut video_reader = H26xSampleReader::new(BufReader::new(file), 1_048_576, false);

    let mut sent = 0;
    loop {
        let sample = match video_reader.next_sample() {
            Ok(sample) => sample,
            Err(webrtc::error::Error::ErrIoEOF) => return Ok(sent),
            Err(e) => return Err(e.into()),
        };

        // No receivers is not an error: the source keeps its pace for whoever joins later
        let _ = tx.send(EncodedFrame {
            data: sample.data,
            timed: sample.timed,
        });
        sent += 1;

        if sample.timed {
            ticker.tick().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "streamer-client-{}-{}-{name}",
            std::process::id(),
            rand::random::<u32>()
        ))
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

    #[tokio::test]
    async fn test_source_sends_headers_then_keyframe() {
        let (tx, mut rx) = broadcast::channel(30);

        // The crate lives in crates/streamer-client, the test video at the workspace root
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(TEST_VIDEO_PATH);
        start_test_source(tx, path.to_str().unwrap().to_owned());

        let mut types = Vec::new();
        for _ in 0..3 {
            let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("source should send a frame in time")
                .expect("channel should stay open");
            types.push(nal_type(&frame.data));
        }

        assert_eq!(types, [Some(7), Some(8), Some(5)]); // SPS, PPS, IDR
    }

    #[tokio::test]
    async fn test_source_paces_timed_frames() {
        let (tx, mut rx) = broadcast::channel(30);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(TEST_VIDEO_PATH);
        start_test_source(tx, path.to_str().unwrap().to_owned());

        // Arrival time of the next timed frame, skipping the untimed SPS/PPS
        async fn next_timed(rx: &mut broadcast::Receiver<EncodedFrame>) -> Instant {
            loop {
                let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("source should send a frame in time")
                    .expect("channel should stay open");
                if frame.timed {
                    return Instant::now();
                }
            }
        }

        // The first tick of an interval completes at once, so the IDR and the first P-frame
        // leave together. The pace shows from the second timed frame to the third.
        next_timed(&mut rx).await;
        let second = next_timed(&mut rx).await;
        let third = next_timed(&mut rx).await;

        let gap = third - second;
        assert!(
            (Duration::from_millis(20)..=Duration::from_millis(80)).contains(&gap),
            "timed frames should be about 33 ms apart, got {gap:?}"
        );
    }

    /// Receives one frame, failing instead of hanging if none comes.
    async fn recv_frame(
        rx: &mut broadcast::Receiver<EncodedFrame>,
    ) -> Result<EncodedFrame, broadcast::error::RecvError> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("source should send or close in time")
    }

    #[tokio::test]
    async fn test_source_starts_over_when_file_ends() {
        // A tiny Annex B stream: SPS, PPS, IDR, P-frame
        let path = temp_file_path("loop.h264");
        let mut stream = Vec::new();
        for nal in [[0x67, 0x42], [0x68, 0xce], [0x65, 0x88], [0x41, 0x9a]] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(&nal);
        }
        std::fs::write(&path, stream).expect("should write the test stream");

        let (tx, mut rx) = broadcast::channel(30);
        start_test_source(tx, path.to_str().unwrap().to_owned());

        let mut types = Vec::new();
        for _ in 0..8 {
            let frame = recv_frame(&mut rx).await.expect("channel should stay open");
            types.push(nal_type(&frame.data));
        }
        let _ = std::fs::remove_file(&path);

        // After the last frame the file starts again with the SPS
        let pass = [Some(7), Some(8), Some(5), Some(1)];
        assert_eq!(types, [pass, pass].concat());
    }

    #[tokio::test]
    async fn test_source_stops_on_empty_file() {
        let path = temp_file_path("empty-source.h264");
        std::fs::write(&path, []).expect("should create empty file");

        let (tx, mut rx) = broadcast::channel(30);
        start_test_source(tx, path.to_str().unwrap().to_owned());

        // The task ends and drops the sender instead of restarting forever
        let result = recv_frame(&mut rx).await;
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Err(broadcast::error::RecvError::Closed)));
    }

    #[tokio::test]
    async fn test_source_stops_on_missing_file() {
        let path = temp_file_path("missing-source.h264");

        let (tx, mut rx) = broadcast::channel(30);
        start_test_source(tx, path.to_str().unwrap().to_owned());

        let result = recv_frame(&mut rx).await;
        assert!(matches!(result, Err(broadcast::error::RecvError::Closed)));
    }

    /// A one-byte frame whose NAL type is `nal`.
    fn frame(nal: u8, timed: bool) -> EncodedFrame {
        EncodedFrame {
            data: Bytes::from(vec![nal]),
            timed,
        }
    }

    fn send(tx: &broadcast::Sender<EncodedFrame>, frame: EncodedFrame) {
        assert!(tx.send(frame).is_ok(), "the receiver should be subscribed");
    }

    const ZERO: Duration = Duration::ZERO;
    const FRAME: Duration = H26X_FRAME_DURATION;

    #[tokio::test]
    async fn test_forward_frames_gates_and_sets_durations() {
        let (tx, rx) = broadcast::channel(16);
        // A P-frame before the SPS, then SPS, PPS, IDR and a P-frame
        for f in [
            frame(0x41, true),
            frame(0x67, false),
            frame(0x68, false),
            frame(0x65, true),
            frame(0x41, true),
        ] {
            send(&tx, f);
        }
        drop(tx);

        let mut written = Vec::new();
        let result = forward_frames(rx, |data: Bytes, duration| {
            written.push((nal_type(&data), duration));
            std::future::ready(Ok::<(), ()>(()))
        })
        .await;

        assert_eq!(result, Ok(()), "a closed source ends the loop cleanly");
        assert_eq!(
            written,
            [
                (Some(7), ZERO),
                (Some(8), ZERO),
                (Some(5), FRAME),
                (Some(1), FRAME)
            ]
        );
    }

    #[tokio::test]
    async fn test_forward_frames_lag_waits_for_next_sps() {
        let (tx, rx) = broadcast::channel(4);
        // The sink reports each write, so the test knows when the gate is open
        let (written_tx, mut written_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(forward_frames(rx, move |data: Bytes, _duration| {
            let _ = written_tx.send(nal_type(&data));
            std::future::ready(Ok::<(), ()>(()))
        }));

        send(&tx, frame(0x67, false));
        send(&tx, frame(0x41, true));
        assert_eq!(written_rx.recv().await, Some(Some(7)));
        assert_eq!(written_rx.recv().await, Some(Some(1)));

        // Six P-frames and then SPS + P-frame into a channel of four: the receiver lags and
        // finds 1, 1, 7, 1. The two P-frames that survived the lag must not be written.
        for _ in 0..6 {
            send(&tx, frame(0x41, true));
        }
        send(&tx, frame(0x67, false));
        send(&tx, frame(0x41, true));
        drop(tx);

        assert_eq!(task.await.expect("task should not panic"), Ok(()));
        let mut after_lag = Vec::new();
        while let Some(nal) = written_rx.recv().await {
            after_lag.push(nal);
        }
        assert_eq!(after_lag, [Some(7), Some(1)]);
    }

    #[tokio::test]
    async fn test_forward_frames_stops_on_sink_error() {
        let (tx, rx) = broadcast::channel(16);
        send(&tx, frame(0x67, false));
        send(&tx, frame(0x68, false));

        let mut calls = 0;
        let result = forward_frames(rx, |_data: Bytes, _duration| {
            calls += 1;
            std::future::ready(Err::<(), _>("write failed"))
        })
        .await;

        assert_eq!(result, Err("write failed"));
        assert_eq!(calls, 1, "nothing is written after the first failure");
        drop(tx);
    }
}
