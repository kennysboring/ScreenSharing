//! Host WebSocket: checks the host token, then relays signaling between the host and the viewers.

use crate::rooms::{
    MAX_MESSAGE_SIZE, SharedState, is_current_host, register_host, remove_viewer, unregister_host,
};
use axum::{
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response,
};
use common::{HostToServer, ServerToHost, ServerToViewer, SignalMessage};
use futures_util::{SinkExt, StreamExt};
use std::time::Instant;
use tokio::sync::mpsc::{Receiver, error::TrySendError};
use uuid::Uuid;

pub async fn ws_host(
    Path(room_id): Path<String>,
    State(state): State<SharedState>,
    header: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<response::Response, StatusCode> {
    let token = extract_token(&header).ok_or(StatusCode::UNAUTHORIZED)?;

    let (receiver, id) = {
        // .lock in

        let mut guard = state.lock().unwrap();

        let room = guard.0.get_mut(&room_id).ok_or(StatusCode::NOT_FOUND)?;

        if room.host_token != token {
            return Err(StatusCode::UNAUTHORIZED);
        }

        register_host(room)
    }; // .lock out

    Ok(ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| web_socket_upgrade_host(socket, receiver, id, state, room_id)))
}

async fn web_socket_upgrade_host(
    socket: WebSocket,
    mut receiver: Receiver<ServerToHost>,
    id_host: Uuid,
    state: SharedState,
    room_id: String,
) {
    println!("WebSocket established");

    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            ws_receiver = receiver.recv() => {
                match ws_receiver {
                    Some(msg) => {
                        let Ok(serialize) = serde_json::to_string(&msg) else { break; };
                        let Ok(_) = sink.send(Message::Text(serialize.into())).await else { break; };
                    },
                    None => break,
                }
            },
            ws_stream = stream.next() => {
                match ws_stream {
                    Some(Ok(Message::Text(msg))) => if !handle_host_message(&msg, &state, &room_id, id_host) { break; },
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    None => break,
                    _ => continue,

                }
            }
        }
    }

    {
        // .lock in
        let mut guard = state.lock().unwrap();

        if let Some(room) = guard.0.get_mut(&room_id) {
            unregister_host(room, id_host, Instant::now());
        }
    } // .lock out
}

fn extract_token(headers: &HeaderMap) -> Option<Uuid> {
    let authorization = headers.get(AUTHORIZATION)?;
    let string = authorization.to_str().ok()?.strip_prefix("Bearer ")?;
    Uuid::parse_str(string).ok()
}

fn handle_host_message(msg: &str, state: &SharedState, room_id: &str, id_host: Uuid) -> bool {
    let (to, signal_message) = match serde_json::from_str::<HostToServer>(msg) {
        Ok(HostToServer::SignalToViewer { to, message }) => match message {
            SignalMessage::Offer { .. } | SignalMessage::Ice { .. } => (to, message),
            SignalMessage::Answer { .. } => return false,
        },
        Err(_) => return false,
    };

    {
        // .lock in
        let guard = state.lock().unwrap();

        let Some(room) = guard.0.get(room_id) else {
            return true;
        };

        if !is_current_host(room, id_host) {
            return false;
        }

        let Some(viewer) = room.viewers.get(&to) else {
            return true;
        };

        let server_message = ServerToViewer::SignalFromHost {
            message: signal_message,
        };

        match viewer.try_send(server_message) {
            Ok(_) => return true,
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Closed(_)) => return true,
        }
    } // .lock out

    remove_viewer(state, room_id, to);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::{AllRooms, Host, Room};
    use axum::http::HeaderValue;
    use common::ViewerId;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::sync::mpsc::{Sender, channel};

    #[test]
    fn test_extract_token_valid_bearer() {
        let mut headers = HeaderMap::new();
        let valid_token = Uuid::new_v4();

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {valid_token}")).unwrap(),
        );

        assert_eq!(extract_token(&headers), Some(valid_token));
    }

    #[test]
    fn test_extract_token_missing_bearer_prefix() {
        let mut headers = HeaderMap::new();
        let valid_token = Uuid::new_v4();

        // Send only the UUID, without the "Bearer " prefix
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&valid_token.to_string()).unwrap(),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_invalid_uuid_after_bearer() {
        let mut headers = HeaderMap::new();

        // Correct "Bearer " prefix, but the content is not a valid UUID
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer garbage-invalid-123"),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_missing_header() {
        // Completely empty HeaderMap (no Authorization)
        let headers = HeaderMap::new();

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_bearer_without_token() {
        let mut headers = HeaderMap::new();

        // "Bearer " with a space but no token
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(extract_token(&headers), None);

        // Just "Bearer", with no space and no token
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_lowercase_bearer() {
        let mut headers = HeaderMap::new();
        let valid_token = Uuid::new_v4();

        // Lowercase "bearer " (the current implementation requires "Bearer " with a capital B)
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("bearer {valid_token}")).unwrap(),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_non_ascii_header_value() {
        let mut headers = HeaderMap::new();

        // Arbitrary non-ASCII bytes make authorization.to_str() return Err
        let non_ascii_bytes = b"Bearer \xFF\xFE\xFD";
        let invalid_header = HeaderValue::from_bytes(non_ascii_bytes).unwrap();
        headers.insert(AUTHORIZATION, invalid_header);

        assert_eq!(extract_token(&headers), None);
    }

    fn room_with_channels(
        host_id: Uuid,
        host_sender: Sender<ServerToHost>,
        viewer_sender: Sender<ServerToViewer>,
    ) -> SharedState {
        let mut viewers = HashMap::new();
        viewers.insert(ViewerId(0), viewer_sender);

        let room = Room {
            viewers,
            next_viewer_id: 1,
            host: Some(Host {
                id: host_id,
                sender: host_sender,
            }),
            host_token: Uuid::new_v4(),
            host_absent_since: None,
        };

        let mut rooms = HashMap::new();
        rooms.insert("room".to_string(), room);
        Arc::new(Mutex::new(AllRooms(rooms)))
    }

    #[test]
    fn test_handle_host_message_normal_forwarding() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":0,"message":{"type":"offer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "room", host_id);

        assert!(result);
        let received = viewer_rx.try_recv().expect("viewer should receive message");
        assert_eq!(
            received,
            ServerToViewer::SignalFromHost {
                message: SignalMessage::Offer {
                    sdp: "x".to_string(),
                }
            }
        );
        assert!(host_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_answer_rejected() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":0,"message":{"type":"answer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "room", host_id);

        assert!(!result);
        assert!(viewer_rx.try_recv().is_err());
        assert!(host_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_invalid_json() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = "invalid_json";
        let result = handle_host_message(json, &state, "room", host_id);

        assert!(!result);
        assert!(viewer_rx.try_recv().is_err());
        assert!(host_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_nonexistent_viewer() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":99,"message":{"type":"offer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "room", host_id);

        assert!(result);
        assert!(viewer_rx.try_recv().is_err());
        assert!(host_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_replaced_host() {
        let host_id = Uuid::new_v4();
        let different_host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":0,"message":{"type":"offer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "room", different_host_id);

        assert!(!result);
        assert!(viewer_rx.try_recv().is_err());
        assert!(host_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_viewer_channel_full() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);

        // Fill the channel before handing the Sender to the room
        viewer_tx
            .try_send(ServerToViewer::HostUnavailable)
            .expect("should fill viewer channel");

        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":0,"message":{"type":"offer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "room", host_id);

        assert!(result);

        // Check that the viewers map is empty (in a short scope)
        {
            let guard = state.lock().unwrap();
            let room = guard.0.get("room").expect("room should exist");
            assert!(room.viewers.is_empty(), "viewer should have been removed");
        }

        // Host receives ViewerLeft { viewer_id: ViewerId(0) }
        let host_msg = host_rx
            .try_recv()
            .expect("host should receive ViewerLeft notification");
        assert_eq!(
            host_msg,
            ServerToHost::ViewerLeft {
                viewer_id: ViewerId(0)
            }
        );

        // The viewer channel held the initial message it was filled with
        assert_eq!(
            viewer_rx.try_recv().unwrap(),
            ServerToViewer::HostUnavailable
        );
        assert!(viewer_rx.try_recv().is_err());
    }

    #[test]
    fn test_handle_host_message_nonexistent_room() {
        let host_id = Uuid::new_v4();
        let (host_tx, mut host_rx) = channel::<ServerToHost>(8);
        let (viewer_tx, mut viewer_rx) = channel::<ServerToViewer>(1);
        let state = room_with_channels(host_id, host_tx, viewer_tx);

        let json = r#"{"type":"signalToViewer","to":0,"message":{"type":"offer","sdp":"x"}}"#;
        let result = handle_host_message(json, &state, "nonexistent_room", host_id);

        assert!(result);
        assert!(viewer_rx.try_recv().is_err());
        assert!(host_rx.try_recv().is_err());
    }
}
