//! Viewer WebSocket: adds the viewer to the room, then relays its signaling to the host.

use crate::rooms::{CAPACITY_CHANNEL, MAX_MESSAGE_SIZE, SharedState, remove_viewer};
use axum::{
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::Response,
};
use common::{ServerToHost, ServerToViewer, SignalMessage, ViewerId, ViewerToServer};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{Receiver, channel, error::TrySendError};

const MAX_VIEWERS_PER_ROOM: usize = 5;

pub async fn ws_viewer(
    Path(room_id): Path<String>,
    State(state): State<SharedState>,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let (receiver, id) = {
        // .lock in

        let mut guard = state.lock().unwrap();

        let room = guard.0.get_mut(&room_id).ok_or(StatusCode::NOT_FOUND)?;

        if room.viewers.len() >= MAX_VIEWERS_PER_ROOM {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        let (sender, receiver) = channel::<ServerToViewer>(CAPACITY_CHANNEL);

        let id = ViewerId(room.next_viewer_id);
        if let Some(host) = &room.host {
            if host
                .sender
                .try_send(ServerToHost::ViewerJoined { viewer_id: id })
                .is_err()
            {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
        } else {
            let _ = sender.try_send(ServerToViewer::HostUnavailable);
        }

        room.viewers.insert(id, sender);
        room.next_viewer_id += 1;
        (receiver, id)
    }; // .lock out

    let body = ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| web_socket_upgrade_viewer(socket, receiver, id, state, room_id));

    Ok(body)
}

async fn web_socket_upgrade_viewer(
    socket: WebSocket,
    mut receiver: Receiver<ServerToViewer>,
    id: ViewerId,
    state: SharedState,
    room_id: String,
) {
    println!("viewer connected");

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
                    Some(Ok(Message::Text(msg))) => if !handle_viewer_message(&msg, &state, &room_id, id) {break;},
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    None => break,
                    _ => continue,
                }
            }
        }
    }

    remove_viewer(&state, &room_id, id);
}

fn handle_viewer_message(msg: &str, state: &SharedState, room_id: &str, id: ViewerId) -> bool {
    let signal_message = match serde_json::from_str::<ViewerToServer>(msg) {
        Ok(ViewerToServer::SignalToHost { message }) => match message {
            SignalMessage::Answer { .. } | SignalMessage::Ice { .. } => message,
            SignalMessage::Offer { .. } => return false,
        },
        Err(_) => return false,
    };

    {
        // .lock in
        let guard = state.lock().unwrap();

        let Some(room) = guard.0.get(room_id) else {
            return true;
        };
        let Some(host) = &room.host else { return true };

        let server_message = ServerToHost::SignalFromViewer {
            from: id,
            message: signal_message,
        };

        match host.sender.try_send(server_message) {
            Ok(_) => {}
            Err(TrySendError::Full(_)) => return false,
            Err(TrySendError::Closed(_)) => return true,
        }
    } // .lock out
    true
}
