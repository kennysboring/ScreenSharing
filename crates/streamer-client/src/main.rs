use axum::{Json, Router, extract::State, response::Html, routing::get};
use common::{CreateRoomResponse, HostToServer, ServerToHost, SignalMessage, ViewerId};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use rtc::{
    interceptor::Registry,
    media::{Sample, io::h26x_reader::H26xSampleReader},
    media_stream::MediaStreamTrack,
    peer_connection::configuration::media_engine::MIME_TYPE_H264,
    rtp_transceiver::{
        PayloadType,
        rtp_sender::{
            RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
            RtpCodecKind,
        },
    },
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    env::args,
    fs::File,
    io::BufReader,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::time::interval;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error::Http, Message, client::IntoClientRequest, handshake::client::Request},
};
use webrtc::{
    media_stream::{
        Track,
        track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
    },
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent,
        RTCPeerConnectionState, RTCSessionDescription, register_default_interceptors,
    },
    rtp_transceiver::RtpSender,
};

#[derive(Clone)]
struct Handler;

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, _event: RTCPeerConnectionIceEvent) {}
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        println!("state connection: {state}");
    }
}

struct Session {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    answered: bool,
    pending: Vec<RTCIceCandidateInit>,
}

#[derive(Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum PanelStatus {
    Creating,
    Live { viewer_link: String, viewers: usize },
    Error { message: String },
}

enum Phase {
    Creating,
    Live { viewer_link: String },
    Error { message: String },
}

struct Panel {
    phase: Phase,
    viewers: HashSet<ViewerId>,
}

type SharedPanel = Arc<Mutex<Panel>>;

#[tokio::main]
async fn main() -> ExitCode {
    let addr = args().nth(1).expect("use: streamer-client <host:porta>");
    if let Err(msg) = validate_addr(&addr) {
        eprintln!("{msg}");
        return ExitCode::FAILURE;
    }

    let panel: SharedPanel = Arc::new(Mutex::new(Panel {
        phase: Phase::Creating,
        viewers: HashSet::new(),
    }));
    tokio::spawn(run_panel(Arc::clone(&panel)));

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

    let viewer_link = format!("http://{addr}/room/{}", room.room_id);
    {
        // .lock in
        let mut guard = panel.lock().unwrap();

        guard.phase = Phase::Live { viewer_link };
    } // .lock out

    let (mut split_sink, mut split_stream) = ws_stream.split();

    let mut sessions: HashMap<ViewerId, Session> = HashMap::new();
    while let Some(message) = split_stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<ServerToHost>(&text) else {
                    continue;
                };

                if let ServerToHost::SignalFromViewer {
                    from,
                    message: SignalMessage::Answer { sdp },
                } = &msg
                    && let Some(session) = sessions.get_mut(from)
                {
                    let Ok(desc) = RTCSessionDescription::answer(sdp.clone()) else {
                        continue;
                    };
                    match session.pc.set_remote_description(desc).await {
                        Ok(_) => {
                            println!("answer applied");
                            session.answered = true;

                            if let Ok(payload_type) = negotiated_payload_type(&session.sender).await
                            {
                                let track = session.track.clone(); // uma cópia, porque a task vive mais que este bloco
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    if stream_video("test-assets/teste.h264", track, payload_type)
                                        .await
                                        .is_err()
                                    {
                                        println!("Error stream video");
                                    }
                                });
                            } else {
                                println!("no codec negociated");
                            }

                            for candidate in std::mem::take(&mut session.pending) {
                                let _ = session.pc.add_ice_candidate(candidate).await;
                            }
                        }
                        Err(e) => println!("failure to apply the answer {e}"),
                    }
                }

                if let ServerToHost::SignalFromViewer {
                    from,
                    message:
                        SignalMessage::Ice {
                            candidate,
                            sdp_mid,
                            sdp_mline_index,
                            username_fragment,
                        },
                } = &msg
                    && let Some(session) = sessions.get_mut(from)
                {
                    let init = RTCIceCandidateInit {
                        candidate: candidate.clone(),
                        sdp_mid: sdp_mid.clone(),
                        sdp_mline_index: *sdp_mline_index,
                        username_fragment: username_fragment.clone(),
                        ..Default::default()
                    };
                    // se session.answered: add_ice_candidate(init).await, senão session.pending.push(init)
                    if session.answered {
                        let _ = session.pc.add_ice_candidate(init).await;
                    } else {
                        session.pending.push(init)
                    }
                }

                let joined = match &msg {
                    ServerToHost::ViewerJoined { viewer_id } => Some(*viewer_id),
                    _ => None,
                };

                if let ServerToHost::ViewerLeft { viewer_id } = &msg {
                    if let Some(session) = sessions.remove(viewer_id) {
                        let _ = session.pc.close().await;
                    }
                    println!("sessões ativas: {}", sessions.len());
                }

                {
                    //.lock in
                    let mut guard = panel.lock().unwrap();
                    apply_server_message(&mut guard, msg);
                } //.lock out

                if let Some(viewer_id) = joined {
                    let (sdp, session) = match make_offer().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("Error: {e}");
                            continue;
                        }
                    };

                    sessions.insert(viewer_id, session);
                    println!("sessões ativas: {}", sessions.len());

                    let reply = HostToServer::SignalToViewer {
                        to: viewer_id,
                        message: SignalMessage::Offer { sdp },
                    };
                    let Ok(json) = serde_json::to_string(&reply) else {
                        continue;
                    };
                    if split_sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error in WebSocket messages : {e}");
                break;
            }
        }
    }

    ExitCode::SUCCESS
}

async fn run_panel(panel: SharedPanel) {
    let app = Router::new()
        .route("/api/status", get(panel_status))
        .route("/", get(panel_page))
        .with_state(panel);

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:8787").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error not allowed to use the port: {e}");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Error axum::serve: {e}");
    }
}

async fn panel_status(State(panel): State<SharedPanel>) -> Json<PanelStatus> {
    let guard = panel.lock().unwrap();
    match &guard.phase {
        Phase::Creating => Json(PanelStatus::Creating),
        Phase::Live { viewer_link } => Json(PanelStatus::Live {
            viewer_link: viewer_link.clone(),
            viewers: guard.viewers.len(),
        }),
        Phase::Error { message } => Json(PanelStatus::Error {
            message: message.clone(),
        }),
    }
}

async fn panel_page() -> Html<&'static str> {
    Html(include_str!("../static/control.html"))
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

fn apply_server_message(panel: &mut Panel, msg: ServerToHost) {
    match msg {
        ServerToHost::ViewerJoined { viewer_id } => {
            panel.viewers.insert(viewer_id);
        }
        ServerToHost::ViewerLeft { viewer_id } => {
            panel.viewers.remove(&viewer_id);
        }
        ServerToHost::SignalFromViewer { .. } => {}
        ServerToHost::ViewerList { viewers } => panel.viewers = viewers.into_iter().collect(),
    }
}

async fn make_offer() -> Result<(String, Session), Box<dyn std::error::Error>> {
    let config = RTCConfigurationBuilder::default()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }])
        .build();

    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_handler(Arc::new(Handler))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build()
        .await?;

    let ssrc = rand::random::<u32>();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "stream".to_string(),
            "track".to_string(),
            "label".to_string(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: video_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )?);

    let pc: Arc<dyn PeerConnection> = Arc::new(pc);
    let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;

    let offer = pc.create_offer(None).await?;
    let offer_sdp = offer.sdp.clone();
    pc.set_local_description(offer).await?;

    let session: Session = Session {
        pc,
        track,
        sender,
        answered: false,
        pending: Vec::new(),
    };
    Ok((offer_sdp, session))
}

async fn negotiated_payload_type(
    sender: &Arc<dyn RtpSender>,
) -> Result<PayloadType, Box<dyn std::error::Error>> {
    sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .ok_or_else(|| "sender has no codec".into())
}

const H26X_FRAME_DURATION: Duration = Duration::from_millis(33);

async fn stream_video(
    path: &str,
    track: Arc<TrackLocalStaticSample>,
    payload_type: PayloadType,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut video_reader = H26xSampleReader::new(reader, 1_048_576, false);

    let ssrc = *track.ssrcs().await.first().ok_or("track without ssrc")?;

    let mut ticker = interval(H26X_FRAME_DURATION);
    loop {
        let sample = match video_reader.next_sample() {
            Ok(sample) => sample,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data: sample.data,
                duration: if sample.timed {
                    H26X_FRAME_DURATION
                } else {
                    Duration::ZERO
                },
                ..Sample::new(Instant::now())
            })
            .await?;

        if sample.timed {
            let _ = ticker.tick().await;
        }
    }

    Ok(())
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
                .contains("Error to create WebSocket request")
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
        assert!(
            result
                .unwrap_err()
                .starts_with("Server is not working. Error = ")
        );
    }

    fn new_test_panel() -> Panel {
        Panel {
            phase: Phase::Creating,
            viewers: HashSet::new(),
        }
    }

    #[test]
    fn test_apply_server_message_viewer_joined() {
        let mut panel = new_test_panel();

        // 1. Inserir o primeiro viewer
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(1),
            },
        );
        assert_eq!(panel.viewers.len(), 1);
        assert!(panel.viewers.contains(&ViewerId(1)));

        // 2. Inserir um segundo viewer diferente
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(2),
            },
        );
        assert_eq!(panel.viewers.len(), 2);
        assert!(panel.viewers.contains(&ViewerId(1)));
        assert!(panel.viewers.contains(&ViewerId(2)));

        // 3. Inserir novamente um viewer já existente (comportamento de conjunto)
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(1),
            },
        );
        assert_eq!(panel.viewers.len(), 2);
    }

    #[test]
    fn test_apply_server_message_viewer_left() {
        let mut panel = new_test_panel();
        panel.viewers.insert(ViewerId(1));
        panel.viewers.insert(ViewerId(2));

        // 1. Remover um viewer existente
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerLeft {
                viewer_id: ViewerId(1),
            },
        );
        assert_eq!(panel.viewers.len(), 1);
        assert!(!panel.viewers.contains(&ViewerId(1)));
        assert!(panel.viewers.contains(&ViewerId(2)));

        // 2. Remover um viewer que não estava presente (não deve causar pânico)
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerLeft {
                viewer_id: ViewerId(99),
            },
        );
        assert_eq!(panel.viewers.len(), 1);
        assert!(panel.viewers.contains(&ViewerId(2)));
    }

    #[test]
    fn test_apply_server_message_viewer_list() {
        let mut panel = new_test_panel();
        panel.viewers.insert(ViewerId(99)); // viewer anterior

        // 1. ViewerList substitui completamente os viewers anteriores
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerList {
                viewers: vec![ViewerId(1), ViewerId(2), ViewerId(3)],
            },
        );
        let expected: HashSet<ViewerId> = [ViewerId(1), ViewerId(2), ViewerId(3)]
            .into_iter()
            .collect();
        assert_eq!(panel.viewers, expected);

        // 2. ViewerList vazia zera os viewers do painel
        apply_server_message(&mut panel, ServerToHost::ViewerList { viewers: vec![] });
        assert!(panel.viewers.is_empty());
    }

    #[test]
    fn test_apply_server_message_signal_from_viewer_is_ignored() {
        let mut panel = new_test_panel();
        panel.viewers.insert(ViewerId(1));

        // SignalFromViewer não deve alterar os viewers do painel
        apply_server_message(
            &mut panel,
            ServerToHost::SignalFromViewer {
                from: ViewerId(1),
                message: common::SignalMessage::Offer {
                    sdp: "v=0...".to_string(),
                },
            },
        );

        assert_eq!(panel.viewers.len(), 1);
        assert!(panel.viewers.contains(&ViewerId(1)));
    }
}
