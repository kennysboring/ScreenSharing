//! Keyframe requests from the browser.
//!
//! A PLI (Picture Loss Indication, RFC 4585 6.3.1) is RTCP feedback a receiver sends when it can
//! no longer decode the picture: a lost packet broke the chain of references (each P-frame
//! builds on the previous one, so the error lasts until the next IDR), the decoder hit an error,
//! or it started without a keyframe. A FIR (Full Intra Request, RFC 5104) asks for the same, more
//! forcefully. The browser only sends them when the SDP negotiated `nack pli` / `ccm fir`.
//!
//! webrtc-rs hands inbound RTCP to the application only when an interceptor marks it with
//! `DeliverToApplication`; on the default chain it stops at the interceptors. The forwarder below
//! marks PLI and FIR (and drops the rest, which the other interceptors already used), so they
//! show up as `TrackLocalEvent::OnRtcpPacket` on the session's track.

use rtc::{
    interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket},
    rtcp::{
        self,
        payload_feedbacks::{
            full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
        },
    },
    sansio::Protocol,
    shared::error::Error,
};
use std::{collections::VecDeque, time::Instant};

/// Interceptor slot after every built-in one (the last named slot is 13_000), so the forwarder
/// only narrows the RTCP once all the others have seen it.
pub const FORWARDER_SLOT: usize = 14_000;

/// Whether an RTCP packet asks for a keyframe.
pub fn is_keyframe_request(packet: &dyn rtcp::Packet) -> bool {
    let packet = packet.as_any();
    packet.is::<PictureLossIndication>() || packet.is::<FullIntraRequest>()
}

/// Whether a compound RTCP packet contains a keyframe request.
pub fn contains_keyframe_request(packets: &[Box<dyn rtcp::Packet>]) -> bool {
    packets
        .iter()
        .any(|packet| is_keyframe_request(packet.as_ref()))
}

/// Passes inbound PLI and FIR to the application and drops every other inbound RTCP packet.
/// Outbound traffic goes through untouched.
#[derive(Default)]
pub struct KeyframeRequestForwarder {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequestForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<Box<dyn rtcp::Packet>> = packets
                .iter()
                .filter(|packet| is_keyframe_request(packet.as_ref()))
                .cloned()
                .collect();
            if requests.is_empty() {
                // Not the application's business; the interceptors before this one used it
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequestForwarder {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::{interceptor::AttributedPacket, rtcp::receiver_report::ReceiverReport, rtp};

    fn pli() -> Box<dyn rtcp::Packet> {
        Box::new(PictureLossIndication {
            sender_ssrc: 1,
            media_ssrc: 2,
        })
    }

    fn fir() -> Box<dyn rtcp::Packet> {
        Box::new(FullIntraRequest {
            sender_ssrc: 1,
            media_ssrc: 2,
            fir: vec![],
        })
    }

    fn receiver_report() -> Box<dyn rtcp::Packet> {
        Box::new(ReceiverReport::default())
    }

    fn tagged(packet: Packet) -> TaggedPacket {
        TaggedPacket {
            now: Instant::now(),
            transport: Default::default(),
            message: AttributedPacket::from(packet),
        }
    }

    #[test]
    fn test_pli_and_fir_are_keyframe_requests() {
        assert!(contains_keyframe_request(&[pli()]));
        assert!(contains_keyframe_request(&[fir()]));
        // Browsers send compound RTCP: a receiver report first, then the feedback
        assert!(contains_keyframe_request(&[receiver_report(), pli()]));
    }

    #[test]
    fn test_other_rtcp_is_not_a_keyframe_request() {
        assert!(!contains_keyframe_request(&[receiver_report()]));
        assert!(!contains_keyframe_request(&[]));
    }

    #[test]
    fn test_forwarder_keeps_only_keyframe_requests_and_marks_them() {
        let mut forwarder = KeyframeRequestForwarder::default();
        forwarder
            .handle_read(tagged(Packet::Rtcp(vec![receiver_report(), pli()])))
            .unwrap();

        let out = forwarder.poll_read().expect("the PLI should go on");
        assert!(out.message.has(&Attribute::DeliverToApplication));
        let Packet::Rtcp(packets) = &out.message.packet else {
            panic!("expected RTCP");
        };
        assert_eq!(packets.len(), 1, "the receiver report is dropped");
        assert!(is_keyframe_request(packets[0].as_ref()));
        assert!(forwarder.poll_read().is_none());
    }

    #[test]
    fn test_forwarder_drops_rtcp_without_keyframe_requests() {
        let mut forwarder = KeyframeRequestForwarder::default();
        forwarder
            .handle_read(tagged(Packet::Rtcp(vec![receiver_report()])))
            .unwrap();
        assert!(forwarder.poll_read().is_none());
    }

    #[test]
    fn test_forwarder_passes_rtp_through_unmarked() {
        let mut forwarder = KeyframeRequestForwarder::default();
        forwarder
            .handle_read(tagged(Packet::Rtp(rtp::Packet::default())))
            .unwrap();
        let out = forwarder.poll_read().expect("RTP is not filtered");
        assert!(!out.message.has(&Attribute::DeliverToApplication));
        assert!(matches!(out.message.packet, Packet::Rtp(_)));
    }
}
