use common::CreateRoomResponse;
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use std::{env::args, process::ExitCode};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error::Http, client::IntoClientRequest, handshake::client::Request},
};

#[tokio::main]
async fn main() -> ExitCode {
    let addr = args().nth(1).expect("use: streamer-client <host:porta>");
    if let Err(msg) = validate_addr(&addr) {
        eprintln!("{msg}");
        return ExitCode::FAILURE;
    }

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

    let (_, mut split_stream) = ws_stream.split();
    while let Some(message) = split_stream.next().await {
        match message {
            Ok(message) => {
                println!("{message}")
            }
            Err(e) => {
                eprintln!("Error in WebSocket messages : {e}");
                break;
            }
        }
    }

    ExitCode::SUCCESS
}

fn validate_addr(addr: &str) -> Result<(), &'static str> {
    if addr.contains("localhost") {
        return Err("Use 127.0.0.1 instead of localhost");
    }
    Ok(())
}

fn build_ws_request(addr: &str, room: &CreateRoomResponse) -> Result<Request, String> {
    let ws_url = format!("ws://{addr}/ws/host/{}", room.room_id);
    let mut request = ws_url
        .into_client_request()
        .map_err(|e| format!("Error to create WebSocket request: {e}"))?;

    let authorization_value = HeaderValue::from_str(&format!("Bearer {}", room.host_token))
        .map_err(|_| "Invalid Authorization: Bearer <token>".to_string())?;

    request
        .headers_mut()
        .insert(AUTHORIZATION, authorization_value);

    Ok(request)
}

async fn create_room(client: &Client, url: &str) -> Result<CreateRoomResponse, String> {
    let response = match client.post(url).send().await {
        Ok(response) => response,
        Err(e) => return Err(format!("Server is not working. Error = {e}")),
    };

    match response.status() {
        StatusCode::OK => response
            .json::<CreateRoomResponse>()
            .await
            .map_err(|e| format!("Error: {e}")),
        StatusCode::SERVICE_UNAVAILABLE => Err("full rooms, try again later".to_string()),
        other_cases => Err(format!("Error: cases do not defined: {other_cases}")),
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

        assert_eq!(req.uri().to_string(), "ws://127.0.0.1:3000/ws/host/room-abc-123");
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
        assert!(result.unwrap_err().contains("Error to create WebSocket request"));
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

        assert_eq!(result.unwrap_err(), "full rooms, try again later");
    }

    #[tokio::test]
    async fn test_create_room_unexpected_status_404() {
        let (url, _handle) = spawn_mock_http_server("404 Not Found", "text/plain", "").await;

        let client = Client::new();
        let result = create_room(&client, &url).await;

        assert_eq!(
            result.unwrap_err(),
            "Error: cases do not defined: 404 Not Found"
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
        // Conexão em porta que não está ouvindo
        let result = create_room(&client, "http://127.0.0.1:1/rooms").await;

        assert!(result.is_err());
        assert!(result.unwrap_err().starts_with("Server is not working. Error = "));
    }
}
