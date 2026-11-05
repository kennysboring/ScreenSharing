mod panel;
mod peer;
mod signaling;
mod video;

use common::{HostToServer, ServerToHost, SignalMessage, ViewerId};
use futures_util::{SinkExt, StreamExt};
use panel::{Panel, Phase, SharedPanel, apply_server_message, run_panel};
use peer::{Session, make_offer};
use reqwest::Client;
use signaling::{build_ws_request, create_room, validate_addr};
use std::{
    collections::{HashMap, HashSet},
    env::args,
    process::ExitCode,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error::Http, Message},
};
use video::{EncodedFrame, TEST_VIDEO_PATH, start_test_source};
use webrtc::peer_connection::RTCIceCandidateInit;

/// Frames the source can be ahead of the slowest session before that session lags
/// (about 1 s at 30 fps). A lagging session drops frames and waits for the next keyframe.
const FRAME_CHANNEL_CAPACITY: usize = 30;

#[tokio::main]
async fn main() -> ExitCode {
    let addr = args().nth(1).expect("usage: streamer-client <host:port>");
    if let Err(msg) = validate_addr(&addr) {
        eprintln!("{msg}");
        return ExitCode::FAILURE;
    }

    let panel: SharedPanel = Arc::new(Mutex::new(Panel {
        phase: Phase::Creating,
        viewers: HashSet::new(),
    }));
    tokio::spawn(run_panel(Arc::clone(&panel)));

    let http_url = format!("http://{addr}/rooms");

    let client = Client::new();

    let room = match create_room(&client, &http_url).await {
        Ok(room) => {
            println!("http://{addr}/room/{}", room.room_id);
            room
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let request = match build_ws_request(&addr, &room) {
        Ok(req) => req,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let (ws_stream, _) = match connect_async(request).await {
        Ok(pair) => pair,
        Err(Http(resp)) => {
            eprintln!(
                "Server rejected the WebSocket handshake. HTTP status: {}",
                resp.status()
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let viewer_link = format!("http://{addr}/room/{}", room.room_id);
    {
        // .lock in
        let mut guard = panel.lock().unwrap();

        guard.phase = Phase::Live { viewer_link };
    } // .lock out

    let (mut split_sink, mut split_stream) = ws_stream.split();

    // No session listens yet, so the first receiver is dropped: sessions subscribe later
    let (frames_tx, _) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
    start_test_source(frames_tx.clone(), TEST_VIDEO_PATH.to_owned());

    let mut sessions: HashMap<ViewerId, Session> = HashMap::new();
    while let Some(message) = split_stream.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("WebSocket message error: {e}");
                break;
            }
        };

        let Ok(msg) = serde_json::from_str::<ServerToHost>(&text) else {
            continue;
        };

        handle_viewer_signal(&msg, &mut sessions).await;

        let joined = match &msg {
            ServerToHost::ViewerJoined { viewer_id } => Some(*viewer_id),
            _ => None,
        };

        if let ServerToHost::ViewerLeft { viewer_id } = &msg {
            if let Some(session) = sessions.remove(viewer_id) {
                let _ = session.pc.close().await;
            }
            println!("active sessions: {}", sessions.len());
        }

        {
            //.lock in
            let mut guard = panel.lock().unwrap();
            apply_server_message(&mut guard, msg);
        } //.lock out

        if let Some(viewer_id) = joined {
            let Some(offer_json) = start_session(viewer_id, &mut sessions, &frames_tx).await else {
                continue;
            };
            if split_sink
                .send(Message::Text(offer_json.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    }

    ExitCode::SUCCESS
}

/// Passes a viewer's answer or ICE candidate to that viewer's session.
async fn handle_viewer_signal(msg: &ServerToHost, sessions: &mut HashMap<ViewerId, Session>) {
    let ServerToHost::SignalFromViewer { from, message } = msg else {
        return;
    };
    let Some(session) = sessions.get_mut(from) else {
        return;
    };

    match message {
        SignalMessage::Answer { sdp } => session.apply_answer(sdp).await,
        SignalMessage::Ice {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        } => {
            let init = RTCIceCandidateInit {
                candidate: candidate.clone(),
                sdp_mid: sdp_mid.clone(),
                sdp_mline_index: *sdp_mline_index,
                username_fragment: username_fragment.clone(),
                ..Default::default()
            };
            session.add_remote_candidate(init).await;
        }
        SignalMessage::Offer { .. } => {}
    }
}

/// Creates a session for a new viewer and returns the offer message to send to it.
async fn start_session(
    viewer_id: ViewerId,
    sessions: &mut HashMap<ViewerId, Session>,
    frames_tx: &broadcast::Sender<EncodedFrame>,
) -> Option<String> {
    let (sdp, session) = match make_offer(frames_tx.clone()).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Error: {e}");
            return None;
        }
    };

    sessions.insert(viewer_id, session);
    println!("active sessions: {}", sessions.len());

    let reply = HostToServer::SignalToViewer {
        to: viewer_id,
        message: SignalMessage::Offer { sdp },
    };
    serde_json::to_string(&reply).ok()
}
