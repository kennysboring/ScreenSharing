use common::CreateRoomResponse;
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use std::{env::args, process::ExitCode};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error::Http, client::IntoClientRequest},
};

#[tokio::main]
async fn main() -> ExitCode {
    let addr = args().nth(1).expect("use: streamer-client <host:porta>");
    if addr.contains("localhost") {
        eprintln!("Use 127.0.0.1 insted of localhost");
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

    let ws_url = format!("ws://{addr}/ws/host/{}", room.room_id);
    let mut request = match ws_url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error to create WebSocket request: {e}");
            return ExitCode::FAILURE;
        }
    };

    let Ok(authorization_value) = HeaderValue::from_str(&format!("Bearer {}", room.host_token))
    else {
        eprintln!("Invalid Authorization: Bearer <token>");
        return ExitCode::FAILURE;
    };
    request
        .headers_mut()
        .insert(AUTHORIZATION, authorization_value);
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
