use axum::{
    Json, Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response,
    routing::{get, post},
};
use common::{CreateRoomResponse, HostToServer, ServerToHost, ServerToViewer, ViewerId};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use uuid::Uuid;

struct Host {
    id: Uuid,
    sender: Sender<ServerToHost>,
}
struct Room {
    viewers: HashMap<ViewerId, Sender<ServerToViewer>>,
    next_viewer_id: u32,
    host: Option<Host>,
    host_token: Uuid,
    host_absent_since: Option<Instant>,
}

struct AllRooms(HashMap<String, Room>);

enum UserType {
    Host,
    Viewer(ViewerId),
}
struct Connection {
    room_id: String,
    user_type: UserType,
}

type SharedState = Arc<Mutex<AllRooms>>;

#[tokio::main]
async fn main() {
    let mutex: SharedState = Arc::new(Mutex::new(AllRooms(HashMap::new())));

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/rooms", post(create_room))
        .route("/ws/host/{room_id}", get(ws_host))
        .with_state(mutex);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> &'static str {
    "vivo"
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const HOST_ABSENCE_GRACE: Duration = Duration::from_secs(180);
const MAX_ROOMS: usize = 10;

async fn create_room(
    State(state): State<SharedState>,
) -> Result<Json<CreateRoomResponse>, StatusCode> {
    let room_id: String = Uuid::new_v4().to_string();
    let host_token: Uuid = Uuid::new_v4();

    let response = CreateRoomResponse {
        room_id: room_id.clone(),
        host_token: host_token.to_string(),
    };

    {
        // .lock in
        let mut guard = state.lock().unwrap();

        if guard.0.len() >= MAX_ROOMS {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        guard.0.insert(
            room_id,
            Room {
                viewers: HashMap::new(),
                next_viewer_id: 0,
                host: None,
                host_token,
                host_absent_since: Some(Instant::now()),
            },
        );
    } // .lock out

    Ok(Json(response))
}

fn remove_expired_rooms(rooms: &mut HashMap<String, Room>, now: Instant, limit: Duration) {
    rooms.retain(|_id, room| match room.host_absent_since {
        Some(time) => now.saturating_duration_since(time) < limit,
        None => true,
    });
}

const CAPACITY_CHANNEL: usize = 32;
const MAX_MESSAGE_SIZE: usize = 64 * 1024;

async fn ws_host(
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

        let (sender, receiver) = channel::<ServerToHost>(CAPACITY_CHANNEL);

        let id = Uuid::new_v4();
        room.host = Some(Host { id, sender });
        room.host_absent_since = None;
        (receiver, id)
    }; // .lock out

    Ok(ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| web_socket_upgrade(socket, receiver, id, state, room_id)))
}

async fn web_socket_upgrade(
    socket: WebSocket,
    mut receiver: Receiver<ServerToHost>,
    id_host: Uuid,
    state: SharedState,
    room_id: String,
) {
    println!("WebSocket estabelecido");

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
                    Some(Ok(Message::Text(msg))) => {
                        let Ok(_) = serde_json::from_str::<HostToServer>(&msg) else { break; };
                    },
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
            if matches!(&room.host, Some(host) if host.id == id_host) {
                room.host = None;
                room.host_absent_since = Some(Instant::now());
            }
        }
    } // .lock out
}

fn extract_token(headers: &HeaderMap) -> Option<Uuid> {
    let authorization = headers.get(AUTHORIZATION)?;
    let string = authorization.to_str().ok()?.strip_prefix("Bearer ")?;
    Uuid::parse_str(string).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

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

        // Envia apenas o UUID sem o prefixo "Bearer "
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&valid_token.to_string()).unwrap(),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_invalid_uuid_after_bearer() {
        let mut headers = HeaderMap::new();

        // Prefixo "Bearer " correto, mas conteúdo não é um UUID válido
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer lixo-invalido-123"),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_missing_header() {
        // HeaderMap completamente vazio (sem Authorization)
        let headers = HeaderMap::new();

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_bearer_without_token() {
        let mut headers = HeaderMap::new();

        // "Bearer " com espaço mas sem token
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(extract_token(&headers), None);

        // Apenas "Bearer" sem espaço nem token
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_lowercase_bearer() {
        let mut headers = HeaderMap::new();
        let valid_token = Uuid::new_v4();

        // "bearer " em minúsculas (a implementação atual exige "Bearer " com B maiúsculo)
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("bearer {valid_token}")).unwrap(),
        );

        assert_eq!(extract_token(&headers), None);
    }

    #[test]
    fn test_extract_token_non_ascii_header_value() {
        let mut headers = HeaderMap::new();

        // Bytes arbitrários não-ASCII fazem authorization.to_str() retornar Err
        let non_ascii_bytes = b"Bearer \xFF\xFE\xFD";
        let invalid_header = HeaderValue::from_bytes(non_ascii_bytes).unwrap();
        headers.insert(AUTHORIZATION, invalid_header);

        assert_eq!(extract_token(&headers), None);
    }

    fn room_with(host_absent_since: Option<Instant>) -> Room {
        Room {
            viewers: HashMap::new(),
            next_viewer_id: 0,
            host: None,
            host_token: Uuid::new_v4(),
            host_absent_since,
        }
    }

    #[test]
    fn test_remove_expired_rooms_absent_beyond_grace() {
        let mut rooms = HashMap::new();
        let desde = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        rooms.insert("room1".to_string(), room_with(Some(desde)));

        // Ausente além da tolerância (1 segundo a mais)
        let now = desde + grace + Duration::from_secs(1);
        remove_expired_rooms(&mut rooms, now, grace);

        assert!(rooms.is_empty());
    }

    #[test]
    fn test_remove_expired_rooms_absent_within_grace() {
        let mut rooms = HashMap::new();
        let desde = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        rooms.insert("room1".to_string(), room_with(Some(desde)));

        // Ausente menos que a tolerância
        let now = desde + Duration::from_secs(100);
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("room1"));
    }

    #[test]
    fn test_remove_expired_rooms_host_present() {
        let mut rooms = HashMap::new();
        let desde = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // Host presente (host_absent_since = None)
        rooms.insert("room1".to_string(), room_with(None));

        // Mesmo com `now` muito no futuro (ex: 100 horas depois)
        let now = desde + Duration::from_secs(360_000);
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("room1"));
    }

    #[test]
    fn test_remove_expired_rooms_exact_boundary_and_transition() {
        let mut rooms = HashMap::new();
        let desde = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // 1. Instante imediatamente anterior à tolerância (now = desde + grace - 1 ns): deve manter
        rooms.insert("room1".to_string(), room_with(Some(desde)));
        let now_before = desde + grace - Duration::from_nanos(1);
        remove_expired_rooms(&mut rooms, now_before, grace);
        assert_eq!(
            rooms.len(),
            1,
            "A sala deveria ser mantida 1ns antes de atingir a tolerância exata"
        );

        // 2. Fronteira exata (now = desde + grace): com '<', o delta é igual ao limit, resultando em false -> removida
        let now_exact = desde + grace;
        remove_expired_rooms(&mut rooms, now_exact, grace);
        assert!(
            rooms.is_empty(),
            "A sala deveria ser removida no instante exato da tolerância (now == desde + grace)"
        );
    }

    #[test]
    fn test_remove_expired_rooms_isolates_expired_entry() {
        let mut rooms = HashMap::new();
        let desde = Instant::now();
        let grace = HOST_ABSENCE_GRACE;

        // Sala 1: ausente desde 'desde' (vai expirar no tempo 'now')
        rooms.insert("expired_room".to_string(), room_with(Some(desde)));

        // Sala 2: ausente mais recentemente (permanecerá válida)
        let recente = desde + Duration::from_secs(100);
        rooms.insert("valid_room".to_string(), room_with(Some(recente)));

        let now = desde + grace;
        remove_expired_rooms(&mut rooms, now, grace);

        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains_key("valid_room"));
        assert!(!rooms.contains_key("expired_room"));
    }
}
