use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use common::{CreateRoomResponse, ServerToHost, ServerToViewer, ViewerId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

struct Room {
    host: Option<Sender<ServerToHost>>,
    viewers: HashMap<ViewerId, Sender<ServerToViewer>>,
    next_viewer_id: u32,
    host_token: Uuid,
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
        .with_state(mutex);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> &'static str {
    "vivo"
}

const LIMIT_USERS: usize = 10;

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

        if guard.0.len() == LIMIT_USERS {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        guard.0.insert(
            room_id,
            Room {
                host: None,
                viewers: HashMap::new(),
                next_viewer_id: 0,
                host_token,
            },
        );
    } // .lock out

    Ok(Json(response))
}
