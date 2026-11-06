//! WebRTC session with one viewer: creates the offer, applies the answer and the ICE candidates.

use crate::keyframe::{FORWARDER_SLOT, KeyframeRequestForwarder};
use crate::video::{self, FrameSource};
use rtc::{
    interceptor::{Registry, Slot},
    media_stream::MediaStreamTrack,
    peer_connection::configuration::media_engine::MIME_TYPE_H264,
    rtp_transceiver::{
        PayloadType,
        rtp_sender::{
            RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
            RTCRtpEncodingParameters, RtpCodecKind,
        },
    },
};
use std::{sync::Arc, time::Instant};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use webrtc::{
    media_stream::track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent,
        RTCPeerConnectionState, RTCSessionDescription, register_default_interceptors,
    },
    rtp_transceiver::RtpSender,
};

#[derive(Clone)]
struct Handler {
    connected_tx: Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, _event: RTCPeerConnectionIceEvent) {}
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected_tx.try_send(());
        }
        println!("connection state: {state}");
    }
}

pub struct Session {
    pub pc: Arc<dyn PeerConnection>,
    pub track: Arc<TrackLocalStaticSample>,
    pub sender: Arc<dyn RtpSender>,
    pub answered: bool,
    pub pending: Vec<RTCIceCandidateInit>,
    pub connected_rx: Option<Receiver<()>>,
    /// The shared frame source. The session subscribes to it once the connection is up.
    pub source: FrameSource,
}

impl Session {
    /// Applies the viewer's answer, starts sending the shared video and adds the ICE candidates
    /// that arrived before the answer.
    pub async fn apply_answer(&mut self, sdp: &str) {
        let Ok(desc) = RTCSessionDescription::answer(sdp.to_owned()) else {
            return;
        };
        match self.pc.set_remote_description(desc).await {
            Ok(_) => {
                println!("answer applied");
                self.answered = true;

                if let Ok(payload_type) = negotiated_payload_type(&self.sender).await {
                    if let Some(rx) = self.connected_rx.take() {
                        video::start_session_video(
                            self.track.clone(),
                            payload_type,
                            rx,
                            self.source.clone(),
                        );
                    }
                } else {
                    println!("no codec negotiated");
                }

                for candidate in std::mem::take(&mut self.pending) {
                    let _ = self.pc.add_ice_candidate(candidate).await;
                }
            }
            Err(e) => println!("failed to apply the answer: {e}"),
        }
    }

    /// Adds the candidate now if the answer was applied, or keeps it until then.
    pub async fn add_remote_candidate(&mut self, init: RTCIceCandidateInit) {
        if self.answered {
            let _ = self.pc.add_ice_candidate(init).await;
        } else {
            self.pending.push(init)
        }
    }
}

/// H.264 fmtp for Constrained Baseline at `level` (level_idc). profile-level-id is
/// profile_idc 0x42 (Baseline), constraint flags 0xe0 (set0, set1 = Constrained, set2) and the
/// level. Codecs match on the first two bytes only, so the level can differ from the browser's;
/// with level-asymmetry-allowed each side's level is what that side can receive.
fn h264_fmtp_line(level: u8) -> String {
    format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e0{level:02x}")
}

pub async fn make_offer(
    source: FrameSource,
) -> Result<(String, Session), Box<dyn std::error::Error>> {
    let config = RTCConfigurationBuilder::default()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }])
        .build();

    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: h264_fmtp_line(source.h264_level()),
            // Lets the browser ask for a keyframe (PLI, FIR) when it loses the picture
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "pli".to_owned(),
                },
                RTCPFeedback {
                    typ: "ccm".to_owned(),
                    parameter: "fir".to_owned(),
                },
            ],
        },
        payload_type: 102,
    };

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
    // Last in the chain: hands PLI/FIR to the session as track events
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?.with(
        Slot::from(FORWARDER_SLOT),
        KeyframeRequestForwarder::default(),
    );

    let (connected_tx, connected_rx) = channel::<()>(1);

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_handler(Arc::new(Handler { connected_tx }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build()
        .await?;

    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "stream".to_string(),
            "track".to_string(),
            "label".to_string(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: video_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )?);

    let pc: Arc<dyn PeerConnection> = Arc::new(pc);
    let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;

    let offer = pc.create_offer(None).await?;
    let offer_sdp = offer.sdp.clone();
    pc.set_local_description(offer).await?;

    let session: Session = Session {
        pc,
        track,
        sender,
        answered: false,
        pending: Vec::new(),
        connected_rx: Some(connected_rx),
        source,
    };
    Ok((offer_sdp, session))
}

async fn negotiated_payload_type(
    sender: &Arc<dyn RtpSender>,
) -> Result<PayloadType, Box<dyn std::error::Error>> {
    sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .ok_or_else(|| "sender has no codec".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;
    use webrtc::media_stream::Track;

    /// A source nobody listens to and that cannot produce keyframes, at the test file's level.
    fn test_source() -> FrameSource {
        source_at_level(crate::video::TEST_VIDEO_H264_LEVEL)
    }

    fn source_at_level(level: u8) -> FrameSource {
        let (frames, _) = broadcast::channel(1);
        FrameSource::new(frames, None, level)
    }

    #[test]
    fn test_h264_fmtp_line_carries_the_level() {
        assert_eq!(
            h264_fmtp_line(0x1f),
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        );
        assert!(h264_fmtp_line(40).ends_with("profile-level-id=42e028"));
    }

    #[tokio::test]
    async fn test_make_offer_announces_the_source_level() {
        let (sdp, session) = make_offer(source_at_level(40))
            .await
            .expect("should create offer");
        assert!(sdp.contains("profile-level-id=42e028"));
        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_make_offer_negotiates_h264_video() {
        let (sdp, session) = make_offer(test_source())
            .await
            .expect("should create offer");

        assert!(sdp.contains("m=video"));
        assert!(sdp.contains("H264/90000"));
        assert!(sdp.contains("profile-level-id=42e01f"));
        assert!(sdp.contains("packetization-mode=1"));
        // The browser may only send the keyframe requests the offer allows
        assert!(sdp.contains("a=rtcp-fb:102 nack pli"));
        assert!(sdp.contains("a=rtcp-fb:102 ccm fir"));
        assert!(!session.answered);
        assert!(session.pending.is_empty());

        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_make_offer_track_has_ssrc() {
        let (_sdp, session) = make_offer(test_source())
            .await
            .expect("should create offer");

        let ssrcs = session.track.ssrcs().await;
        assert_eq!(ssrcs.len(), 1);

        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_negotiated_payload_type_uses_registered_h264() {
        let (_sdp, session) = make_offer(test_source())
            .await
            .expect("should create offer");

        let payload_type = negotiated_payload_type(&session.sender)
            .await
            .expect("sender should have a codec");
        assert_eq!(payload_type, 102);

        let _ = session.pc.close().await;
    }
}
