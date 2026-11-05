//! WebRTC session with one viewer: creates the offer, applies the answer and the ICE candidates.

use crate::video;
use rtc::{
    interceptor::Registry,
    media_stream::MediaStreamTrack,
    peer_connection::configuration::media_engine::MIME_TYPE_H264,
    rtp_transceiver::{
        PayloadType,
        rtp_sender::{
            RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
            RtpCodecKind,
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
}

impl Session {
    /// Applies the viewer's answer, starts the test video and adds the ICE candidates that
    /// arrived before the answer.
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
                        video::start_test_video(self.track.clone(), payload_type, rx);
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

pub async fn make_offer() -> Result<(String, Session), Box<dyn std::error::Error>> {
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
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

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
    use webrtc::media_stream::Track;

    #[tokio::test]
    async fn test_make_offer_negotiates_h264_video() {
        let (sdp, session) = make_offer().await.expect("should create offer");

        assert!(sdp.contains("m=video"));
        assert!(sdp.contains("H264/90000"));
        assert!(sdp.contains("profile-level-id=42e01f"));
        assert!(sdp.contains("packetization-mode=1"));
        assert!(!session.answered);
        assert!(session.pending.is_empty());

        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_make_offer_track_has_ssrc() {
        let (_sdp, session) = make_offer().await.expect("should create offer");

        let ssrcs = session.track.ssrcs().await;
        assert_eq!(ssrcs.len(), 1);

        let _ = session.pc.close().await;
    }

    #[tokio::test]
    async fn test_negotiated_payload_type_uses_registered_h264() {
        let (_sdp, session) = make_offer().await.expect("should create offer");

        let payload_type = negotiated_payload_type(&session.sender)
            .await
            .expect("sender should have a codec");
        assert_eq!(payload_type, 102);

        let _ = session.pc.close().await;
    }
}
