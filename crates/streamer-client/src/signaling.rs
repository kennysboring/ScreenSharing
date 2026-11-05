//! Talks to the signaling server: creates the room and builds the host WebSocket request.

use common::CreateRoomResponse;
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, handshake::client::Request};

pub fn validate_addr(addr: &str) -> Result<(), &'static str> {
    if addr.contains("localhost") {
        return Err("Use 127.0.0.1 instead of localhost");
    }
    Ok(())
}

pub fn build_ws_request(addr: &str, room: &CreateRoomResponse) -> Result<Request, String> {
    let ws_url = format!("ws://{addr}/ws/host/{}", room.room_id);
    let mut request = ws_url
        .into_client_request()
        .map_err(|e| format!("Failed to create WebSocket request: {e}"))?;

    let authorization_value = HeaderValue::from_str(&format!("Bearer {}", room.host_token))
        .map_err(|_| "Invalid Authorization: Bearer <token>".to_string())?;

    request
        .headers_mut()
        .insert(AUTHORIZATION, authorization_value);

    Ok(request)
}

pub async fn create_room(client: &Client, url: &str) -> Result<CreateRoomResponse, String> {
    let response = match client.post(url).send().await {
        Ok(response) => response,
        Err(e) => return Err(format!("Server is not responding. Error: {e}")),
    };

    match response.status() {
        StatusCode::OK => response
            .json::<CreateRoomResponse>()
            .await
            .map_err(|e| format!("Error: {e}")),
        StatusCode::SERVICE_UNAVAILABLE => Err("all rooms are full, try again later".to_string()),
        other_status => Err(format!("Error: unexpected status: {other_status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_validate_addr_rejects_localhost() {
        assert_eq!(
            validate_addr("localhost:3000"),
            Err("Use 127.0.0.1 instead of localhost")
        );
        assert_eq!(
            validate_addr("http://localhost/rooms"),
            Err("Use 127.0.0.1 instead of localhost")
        );
    }

    #[test]
    fn test_validate_addr_accepts_valid_addresses() {
        assert!(validate_addr("127.0.0.1:3000").is_ok());
        assert!(validate_addr("192.168.1.100:8080").is_ok());
        assert!(validate_addr("screensharing.example.com:443").is_ok());
    }

    #[test]
    fn test_build_ws_request_success() {
        let room = CreateRoomResponse {
            room_id: "room-abc-123".to_string(),
            host_token: "token-xyz-456".to_string(),
        };

        let req = build_ws_request("127.0.0.1:3000", &room).expect("should build ws request");

        assert_eq!(
            req.uri().to_string(),
            "ws://127.0.0.1:3000/ws/host/room-abc-123"
        );
        assert_eq!(
            req.headers().get(AUTHORIZATION),
            Some(&HeaderValue::from_static("Bearer token-xyz-456"))
        );
    }

    #[test]
    fn test_build_ws_request_invalid_token() {
        let room = CreateRoomResponse {
            room_id: "room-abc-123".to_string(),
            host_token: "invalid\ntoken\r\n".to_string(),
        };

        let result = build_ws_request("127.0.0.1:3000", &room);
        assert_eq!(
            result.err(),
            Some("Invalid Authorization: Bearer <token>".to_string())
        );
    }

    #[test]
    fn test_build_ws_request_invalid_addr() {
        let room = CreateRoomResponse {
            room_id: "room-1".to_string(),
            host_token: "token-1".to_string(),
        };

        let result = build_ws_request("invalid uri with spaces:80", &room);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Failed to create WebSocket request")
        );
    }

    async fn spawn_mock_http_server(
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind mock server");
        let addr = listener.local_addr().expect("should get local address");
        let url = format!("http://{addr}/rooms");

        let handle = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        (url, handle)
    }

    #[tokio::test]
    async fn test_create_room_success_200_ok() {
        let (url, _handle) = spawn_mock_http_server(
            "200 OK",
            "application/json",
            r#"{"roomId":"room-123","hostToken":"token-456"}"#,
        )
        .await;

        let client = Client::new();
        let result = create_room(&client, &url).await;

        assert_eq!(
            result.unwrap(),
            CreateRoomResponse {
                room_id: "room-123".to_string(),
                host_token: "token-456".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn test_create_room_service_unavailable_503() {
        let (url, _handle) =
            spawn_mock_http_server("503 Service Unavailable", "text/plain", "").await;

        let client = Client::new();
        let result = create_room(&client, &url).await;

        assert_eq!(result.unwrap_err(), "all rooms are full, try again later");
    }

    #[tokio::test]
    async fn test_create_room_unexpected_status_404() {
        let (url, _handle) = spawn_mock_http_server("404 Not Found", "text/plain", "").await;

        let client = Client::new();
        let result = create_room(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            "Error: unexpected status: 404 Not Found"
        );
    }

    #[tokio::test]
    async fn test_create_room_invalid_json_body() {
        let (url, _handle) =
            spawn_mock_http_server("200 OK", "application/json", "not-valid-json").await;

        let client = Client::new();
        let result = create_room(&client, &url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().starts_with("Error: "));
    }

    #[tokio::test]
    async fn test_create_room_connection_failure() {
        let client = Client::new();
        // Connect to a port that nothing is listening on
        let result = create_room(&client, "http://127.0.0.1:1/rooms").await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .starts_with("Server is not responding. Error: ")
        );
    }
}
