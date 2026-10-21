use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
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
        .route("/ws/host/{room_id}", get(ws_host))
        .with_state(mutex);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, app).await.unwrap();
}

async fn ws_host() {
    todo!()
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
}
