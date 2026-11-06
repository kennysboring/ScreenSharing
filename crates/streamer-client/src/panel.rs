//! Local control panel: shows the room state and the viewer count at http://127.0.0.1:8787.

use axum::{Json, Router, extract::State, response::Html, routing::get};
use common::{ServerToHost, ViewerId};
use serde::Serialize;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

#[derive(Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PanelStatus {
    Creating,
    Live { viewer_link: String, viewers: usize },
    Error { message: String },
}

pub enum Phase {
    Creating,
    Live {
        viewer_link: String,
    },
    // TODO: Used when reconnecting with backoff (connection signal dropped)
    #[allow(dead_code)]
    Error {
        message: String,
    },
}

pub struct Panel {
    pub phase: Phase,
    pub viewers: HashSet<ViewerId>,
}

pub type SharedPanel = Arc<Mutex<Panel>>;

pub async fn run_panel(panel: SharedPanel) {
    let app = Router::new()
        .route("/api/status", get(panel_status))
        .route("/", get(panel_page))
        .with_state(panel);

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:8787").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error: could not bind the port: {e}");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Error in axum::serve: {e}");
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

pub fn apply_server_message(panel: &mut Panel, msg: ServerToHost) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn new_test_panel() -> Panel {
        Panel {
            phase: Phase::Creating,
            viewers: HashSet::new(),
        }
    }

    #[test]
    fn test_apply_server_message_viewer_joined() {
        let mut panel = new_test_panel();

        // 1. Insert the first viewer
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(1),
            },
        );
        assert_eq!(panel.viewers.len(), 1);
        assert!(panel.viewers.contains(&ViewerId(1)));

        // 2. Insert a second, different viewer
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(2),
            },
        );
        assert_eq!(panel.viewers.len(), 2);
        assert!(panel.viewers.contains(&ViewerId(1)));
        assert!(panel.viewers.contains(&ViewerId(2)));

        // 3. Insert an existing viewer again (set behavior)
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

        // 1. Remove an existing viewer
        apply_server_message(
            &mut panel,
            ServerToHost::ViewerLeft {
                viewer_id: ViewerId(1),
            },
        );
        assert_eq!(panel.viewers.len(), 1);
        assert!(!panel.viewers.contains(&ViewerId(1)));
        assert!(panel.viewers.contains(&ViewerId(2)));

        // 2. Remove a viewer that was not there (must not panic)
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
        panel.viewers.insert(ViewerId(99)); // previous viewer

        // 1. ViewerList fully replaces the previous viewers
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

        // 2. An empty ViewerList clears the panel viewers
        apply_server_message(&mut panel, ServerToHost::ViewerList { viewers: vec![] });
        assert!(panel.viewers.is_empty());
    }

    #[test]
    fn test_apply_server_message_signal_from_viewer_is_ignored() {
        let mut panel = new_test_panel();
        panel.viewers.insert(ViewerId(1));

        // SignalFromViewer must not change the panel viewers
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

    fn shared_panel(phase: Phase, viewers: &[ViewerId]) -> SharedPanel {
        Arc::new(Mutex::new(Panel {
            phase,
            viewers: viewers.iter().copied().collect(),
        }))
    }

    async fn status_json(panel: SharedPanel) -> serde_json::Value {
        let Json(status) = panel_status(State(panel)).await;
        serde_json::to_value(status).expect("status should serialize")
    }

    #[tokio::test]
    async fn test_panel_status_creating() {
        let panel = shared_panel(Phase::Creating, &[ViewerId(1)]);

        assert_eq!(
            status_json(panel).await,
            serde_json::json!({ "state": "creating" })
        );
    }

    #[tokio::test]
    async fn test_panel_status_live_counts_viewers() {
        let panel = shared_panel(
            Phase::Live {
                viewer_link: "http://127.0.0.1:3000/room/abc".to_string(),
            },
            &[ViewerId(1), ViewerId(2)],
        );

        assert_eq!(
            status_json(panel).await,
            serde_json::json!({
                "state": "live",
                "viewerLink": "http://127.0.0.1:3000/room/abc",
                "viewers": 2
            })
        );
    }

    #[tokio::test]
    async fn test_panel_status_error() {
        let panel = shared_panel(
            Phase::Error {
                message: "boom".to_string(),
            },
            &[],
        );

        assert_eq!(
            status_json(panel).await,
            serde_json::json!({ "state": "error", "message": "boom" })
        );
    }
}
