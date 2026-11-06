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
use video::{EncodedFrame, FrameSource, TEST_VIDEO_PATH, start_test_source};
use webrtc::peer_connection::RTCIceCandidateInit;

/// Frames the source can be ahead of the slowest session before that session lags
/// (about 1 s at 30 fps). A lagging session drops frames and waits for the next keyframe.
const FRAME_CHANNEL_CAPACITY: usize = 30;

const USAGE: &str = "usage: streamer-client <host:port> [--source file|vaapi-test]";

/// Where the video frames come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    /// The pre-encoded test file, looped (no GPU needed)
    File,
    /// Synthetic frames encoded live by VA-API (build with `--features vaapi`, kernel 6.18 only)
    VaapiTest,
}

/// Reads `<host:port> [--source file|vaapi-test]` (the first item is the program name).
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(String, SourceKind), String> {
    let mut args = args.into_iter().skip(1);
    let addr = args.next().ok_or(USAGE)?;
    let mut source = SourceKind::File;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => {
                source = match args.next().as_deref() {
                    Some("file") => SourceKind::File,
                    Some("vaapi-test") => SourceKind::VaapiTest,
                    other => return Err(format!("unknown source {other:?}\n{USAGE}")),
                }
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok((addr, source))
}

/// Starts the chosen frame source publishing on `frames`.
fn start_source(
    kind: SourceKind,
    frames: broadcast::Sender<EncodedFrame>,
) -> Result<FrameSource, String> {
    match kind {
        SourceKind::File => {
            start_test_source(frames.clone(), TEST_VIDEO_PATH.to_owned());
            Ok(FrameSource::new(frames, None))
        }
        #[cfg(feature = "vaapi")]
        SourceKind::VaapiTest => video::start_vaapi_test_source(frames),
        #[cfg(not(feature = "vaapi"))]
        SourceKind::VaapiTest => {
            drop(frames);
            Err("this build has no VA-API support; rebuild with --features vaapi".to_owned())
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let (addr, source_kind) = match parse_args(args()) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(msg) = validate_addr(&addr) {
        eprintln!("{msg}");
        return ExitCode::FAILURE;
    }

    // Started first so a source that cannot run (a VA-API error, the wrong kernel) stops the
    // program before a room is created. No session listens yet, so the first receiver is
    // dropped: sessions subscribe later.
    let (frames_tx, _) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
    let source = match start_source(source_kind, frames_tx) {
        Ok(source) => source,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

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
            let Some(offer_json) = start_session(viewer_id, &mut sessions, &source).await else {
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
    source: &FrameSource,
) -> Option<String> {
    let (sdp, session) = match make_offer(source.clone()).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<(String, SourceKind), String> {
        parse_args(
            std::iter::once("streamer-client")
                .chain(args.iter().copied())
                .map(String::from),
        )
    }

    #[test]
    fn test_parse_args_defaults_to_the_file_source() {
        assert_eq!(
            parse(&["127.0.0.1:3000"]),
            Ok(("127.0.0.1:3000".to_owned(), SourceKind::File))
        );
    }

    #[test]
    fn test_parse_args_reads_the_source() {
        assert_eq!(
            parse(&["127.0.0.1:3000", "--source", "vaapi-test"]),
            Ok(("127.0.0.1:3000".to_owned(), SourceKind::VaapiTest))
        );
        assert_eq!(
            parse(&["127.0.0.1:3000", "--source", "file"]),
            Ok(("127.0.0.1:3000".to_owned(), SourceKind::File))
        );
    }

    #[test]
    fn test_parse_args_rejects_bad_input() {
        assert!(parse(&[]).is_err(), "missing address");
        assert!(
            parse(&["127.0.0.1:3000", "--source"]).is_err(),
            "missing source"
        );
        assert!(parse(&["127.0.0.1:3000", "--source", "camera"]).is_err());
        assert!(parse(&["127.0.0.1:3000", "--verbose"]).is_err());
    }

    #[cfg(not(feature = "vaapi"))]
    #[test]
    fn test_vaapi_source_needs_the_feature() {
        let (frames, _) = broadcast::channel(1);
        let err = start_source(SourceKind::VaapiTest, frames).err();
        assert!(err.is_some_and(|e| e.contains("--features vaapi")));
    }
}
