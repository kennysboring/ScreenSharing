mod api;
mod host;
mod rooms;
mod viewer;

use axum::{
    Router,
    routing::{get, post},
};
use rooms::{AllRooms, SharedState, sweep_expired_rooms};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[tokio::main]
async fn main() {
    let mutex: SharedState = Arc::new(Mutex::new(AllRooms(HashMap::new())));

    tokio::spawn(sweep_expired_rooms(Arc::clone(&mutex)));

    let app = Router::new()
        .route("/health", get(api::health_check))
        .route("/rooms", post(api::create_room))
        .route("/room/{room_id}", get(api::get_html))
        .route("/ws/host/{room_id}", get(host::ws_host))
        .route("/ws/viewer/{room_id}", get(viewer::ws_viewer))
        .with_state(mutex);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, app).await.unwrap();
}
