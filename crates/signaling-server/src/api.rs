//! Plain HTTP routes: health check, room creation and the viewer page.

use crate::rooms::{Room, SharedState};
use axum::{Json, extract::State, http::StatusCode, response::Html};
use common::CreateRoomResponse;
use std::{collections::HashMap, time::Instant};
use uuid::Uuid;

const MAX_ROOMS: usize = 10;

pub async fn health_check() -> &'static str {
    "alive"
}

pub async fn create_room(
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

pub async fn get_html() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}
