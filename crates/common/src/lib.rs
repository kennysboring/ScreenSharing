use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SignalMessage {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,

        #[serde(skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,

        #[serde(rename = "sdpMLineIndex", skip_serializing_if = "Option::is_none")]
        sdp_mline_index: Option<u16>,

        #[serde(skip_serializing_if = "Option::is_none")]
        username_fragment: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct ViewerId(pub u32);

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerToHost {
    ViewerJoined {
        viewer_id: ViewerId,
    },
    ViewerLeft {
        viewer_id: ViewerId,
    },
    SignalFromViewer {
        from: ViewerId,
        message: SignalMessage,
    },
    ViewerList {
        viewers: Vec<ViewerId>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostToServer {
    SignalToViewer {
        to: ViewerId,
        message: SignalMessage,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ViewerToServer {
    SignalToHost { message: SignalMessage },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerToViewer {
    SignalFromHost { message: SignalMessage },
    HostUnavailable,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoomResponse {
    pub room_id: String,
    pub host_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offer_serialization_and_deserialization() {
        let msg = SignalMessage::Offer {
            sdp: "test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"offer","sdp":"test"}"#);

        let deserialized: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_answer_serialization_and_deserialization() {
        let msg = SignalMessage::Answer {
            sdp: "test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"answer","sdp":"test"}"#);

        let deserialized: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_ice_all_fields_present_including_username_fragment() {
        let msg = SignalMessage::Ice {
            candidate: "candidate:1".to_string(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
            username_fragment: Some("ufrag123".to_string()),
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"ice","candidate":"candidate:1","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"ufrag123"}"#
        );

        let deserialized: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_ice_optional_fields_as_explicit_null() {
        // Web browsers often send "sdpMid": null or "usernameFragment": null
        let json = r#"{
            "type": "ice",
            "candidate": "candidate:1",
            "sdpMid": null,
            "sdpMLineIndex": null,
            "usernameFragment": null
        }"#;

        let deserialized: SignalMessage = serde_json::from_str(json).unwrap();
        let expected = SignalMessage::Ice {
            candidate: "candidate:1".to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
            username_fragment: None,
        };

        // Option deserializes JSON `null` as `None`
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_ice_optional_fields_omitted() {
        // Fields missing from the JSON are also deserialized as `None`
        let json = r#"{
            "type": "ice",
            "candidate": "candidate:1"
        }"#;

        let deserialized: SignalMessage = serde_json::from_str(json).unwrap();
        let expected = SignalMessage::Ice {
            candidate: "candidate:1".to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
            username_fragment: None,
        };

        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_ice_serialization_skips_none_fields() {
        // When fields are None, skip_serializing_if keeps them out of the generated JSON
        let msg = SignalMessage::Ice {
            candidate: "candidate:1".to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
            username_fragment: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"ice","candidate":"candidate:1"}"#);
    }

    #[test]
    fn test_deserialize_invalid_type() {
        let json = r#"{"type":"Unknown","sdp":"..."}"#;
        let result = serde_json::from_str::<SignalMessage>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_missing_required_field() {
        // Required field `sdp` missing from offer
        let json_offer_no_sdp = r#"{"type":"offer"}"#;
        let result_offer = serde_json::from_str::<SignalMessage>(json_offer_no_sdp);
        assert!(result_offer.is_err());

        // Required field `candidate` missing from ice
        let json_ice_no_candidate = r#"{"type":"ice"}"#;
        let result_ice = serde_json::from_str::<SignalMessage>(json_ice_no_candidate);
        assert!(result_ice.is_err());
    }

    #[test]
    fn test_deserialize_ignores_unknown_fields() {
        // Serde ignores unmapped fields by default (a protocol choice that tolerates evolution)
        let json = r#"{"type":"offer","sdp":"test","new":1,"extra":{"nested":true}}"#;
        let deserialized = serde_json::from_str::<SignalMessage>(json);
        assert_eq!(
            deserialized.unwrap(),
            SignalMessage::Offer {
                sdp: "test".to_string()
            }
        );
    }

    #[test]
    fn test_viewer_id_transparent_serialization_and_deserialization() {
        let viewer_id = ViewerId(42);
        // A newtype struct serializes directly as the number `42`
        let json = serde_json::to_string(&viewer_id).unwrap();
        assert_eq!(json, "42");

        let deserialized: ViewerId = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, viewer_id);
    }

    #[test]
    fn test_viewer_id_boundaries_and_invalid_deserialization() {
        // Valid values at the u32 bounds
        assert_eq!(serde_json::from_str::<ViewerId>("0").unwrap(), ViewerId(0));
        assert_eq!(
            serde_json::from_str::<ViewerId>("4294967295").unwrap(),
            ViewerId(u32::MAX)
        );

        // A string instead of an integer must fail
        assert!(serde_json::from_str::<ViewerId>(r#""42""#).is_err());
        // u32 overflow (> 4_294_967_295) must fail
        assert!(serde_json::from_str::<ViewerId>("4294967296").is_err());
        // A negative number must fail for an unsigned type
        assert!(serde_json::from_str::<ViewerId>("-1").is_err());
    }

    #[test]
    fn test_server_to_host_viewer_joined_serialization_and_deserialization() {
        let msg = ServerToHost::ViewerJoined {
            viewer_id: ViewerId(7),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"viewerJoined","viewerId":7}"#);

        let deserialized: ServerToHost = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_to_host_viewer_left_serialization_and_deserialization() {
        let msg = ServerToHost::ViewerLeft {
            viewer_id: ViewerId(7),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"viewerLeft","viewerId":7}"#);

        let deserialized: ServerToHost = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_to_host_signal_from_viewer_serialization_and_deserialization() {
        let msg = ServerToHost::SignalFromViewer {
            from: ViewerId(42),
            message: SignalMessage::Offer {
                sdp: "offer_sdp".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"signalFromViewer","from":42,"message":{"type":"offer","sdp":"offer_sdp"}}"#
        );

        let deserialized: ServerToHost = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_to_host_viewer_list_serialization_and_deserialization() {
        // List with elements
        let msg = ServerToHost::ViewerList {
            viewers: vec![ViewerId(1), ViewerId(2), ViewerId(3)],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"viewerList","viewers":[1,2,3]}"#);

        let deserialized: ServerToHost = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);

        // Empty list
        let empty_msg = ServerToHost::ViewerList { viewers: vec![] };
        let empty_json = serde_json::to_string(&empty_msg).unwrap();
        assert_eq!(empty_json, r#"{"type":"viewerList","viewers":[]}"#);

        let deserialized_empty: ServerToHost = serde_json::from_str(&empty_json).unwrap();
        assert_eq!(deserialized_empty, empty_msg);
    }

    #[test]
    fn test_server_to_host_deserialize_invalid_type() {
        let json = r#"{"type":"unknownType","viewerId":1}"#;
        assert!(serde_json::from_str::<ServerToHost>(json).is_err());
    }

    #[test]
    fn test_server_to_host_deserialize_missing_fields() {
        // No viewerId in ViewerJoined
        let json_no_viewer_id = r#"{"type":"viewerJoined"}"#;
        assert!(serde_json::from_str::<ServerToHost>(json_no_viewer_id).is_err());

        // No message in SignalFromViewer
        let json_no_message = r#"{"type":"signalFromViewer","from":1}"#;
        assert!(serde_json::from_str::<ServerToHost>(json_no_message).is_err());

        // No viewers in ViewerList
        let json_no_viewers = r#"{"type":"viewerList"}"#;
        assert!(serde_json::from_str::<ServerToHost>(json_no_viewers).is_err());

        // No type tag
        let json_no_type = r#"{"viewerId":1}"#;
        assert!(serde_json::from_str::<ServerToHost>(json_no_type).is_err());
    }

    #[test]
    fn test_server_to_host_deserialize_ignores_unknown_fields() {
        let json = r#"{"type":"viewerJoined","viewerId":10,"metadata":"extra"}"#;
        let deserialized: ServerToHost = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            ServerToHost::ViewerJoined {
                viewer_id: ViewerId(10)
            }
        );
    }

    #[test]
    fn test_host_to_server_signal_to_viewer_serialization_and_deserialization() {
        let msg = HostToServer::SignalToViewer {
            to: ViewerId(99),
            message: SignalMessage::Answer {
                sdp: "answer_sdp".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"signalToViewer","to":99,"message":{"type":"answer","sdp":"answer_sdp"}}"#
        );

        let deserialized: HostToServer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);

        // Testing with an ICE candidate that has optional fields
        let ice_msg = HostToServer::SignalToViewer {
            to: ViewerId(99),
            message: SignalMessage::Ice {
                candidate: "candidate:1".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: None,
                username_fragment: None,
            },
        };
        let ice_json = serde_json::to_string(&ice_msg).unwrap();
        assert_eq!(
            ice_json,
            r#"{"type":"signalToViewer","to":99,"message":{"type":"ice","candidate":"candidate:1","sdpMid":"0"}}"#
        );

        let deserialized_ice: HostToServer = serde_json::from_str(&ice_json).unwrap();
        assert_eq!(deserialized_ice, ice_msg);
    }

    #[test]
    fn test_host_to_server_deserialize_invalid_type() {
        let json = r#"{"type":"unknownHostMsg","to":1}"#;
        assert!(serde_json::from_str::<HostToServer>(json).is_err());
    }

    #[test]
    fn test_host_to_server_deserialize_missing_fields() {
        // No message
        let json_no_message = r#"{"type":"signalToViewer","to":99}"#;
        assert!(serde_json::from_str::<HostToServer>(json_no_message).is_err());

        // No to
        let json_no_to = r#"{"type":"signalToViewer","message":{"type":"offer","sdp":"test"}}"#;
        assert!(serde_json::from_str::<HostToServer>(json_no_to).is_err());

        // No type tag
        let json_no_type = r#"{"to":99,"message":{"type":"offer","sdp":"test"}}"#;
        assert!(serde_json::from_str::<HostToServer>(json_no_type).is_err());
    }

    #[test]
    fn test_host_to_server_deserialize_ignores_unknown_fields() {
        let json = r#"{"type":"signalToViewer","to":99,"message":{"type":"offer","sdp":"test"},"extra":42}"#;
        let deserialized: HostToServer = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            HostToServer::SignalToViewer {
                to: ViewerId(99),
                message: SignalMessage::Offer {
                    sdp: "test".to_string()
                }
            }
        );
    }

    #[test]
    fn test_viewer_to_server_signal_to_host_serialization_and_deserialization() {
        let msg = ViewerToServer::SignalToHost {
            message: SignalMessage::Offer {
                sdp: "viewer_offer_sdp".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"signalToHost","message":{"type":"offer","sdp":"viewer_offer_sdp"}}"#
        );

        let deserialized: ViewerToServer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_viewer_to_server_deserialize_invalid_type() {
        let json = r#"{"type":"unknownViewerMsg","message":{"type":"offer","sdp":"test"}}"#;
        assert!(serde_json::from_str::<ViewerToServer>(json).is_err());
    }

    #[test]
    fn test_viewer_to_server_deserialize_missing_fields() {
        // No message
        let json_no_message = r#"{"type":"signalToHost"}"#;
        assert!(serde_json::from_str::<ViewerToServer>(json_no_message).is_err());

        // No type tag
        let json_no_type = r#"{"message":{"type":"offer","sdp":"test"}}"#;
        assert!(serde_json::from_str::<ViewerToServer>(json_no_type).is_err());
    }

    #[test]
    fn test_viewer_to_server_deserialize_ignores_unknown_fields() {
        let json = r#"{"type":"signalToHost","message":{"type":"offer","sdp":"test"},"extra":123}"#;
        let deserialized: ViewerToServer = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            ViewerToServer::SignalToHost {
                message: SignalMessage::Offer {
                    sdp: "test".to_string()
                }
            }
        );
    }

    #[test]
    fn test_server_to_viewer_signal_from_host_serialization_and_deserialization() {
        let msg = ServerToViewer::SignalFromHost {
            message: SignalMessage::Answer {
                sdp: "host_answer_sdp".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"signalFromHost","message":{"type":"answer","sdp":"host_answer_sdp"}}"#
        );

        let deserialized: ServerToViewer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_to_viewer_host_unavailable_serialization_and_deserialization() {
        let msg = ServerToViewer::HostUnavailable;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"hostUnavailable"}"#);

        let deserialized: ServerToViewer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_to_viewer_deserialize_invalid_type() {
        let json = r#"{"type":"unknownServerMsg"}"#;
        assert!(serde_json::from_str::<ServerToViewer>(json).is_err());
    }

    #[test]
    fn test_server_to_viewer_deserialize_missing_fields() {
        // SignalFromHost without message
        let json_no_message = r#"{"type":"signalFromHost"}"#;
        assert!(serde_json::from_str::<ServerToViewer>(json_no_message).is_err());

        // No type tag
        let json_no_type = r#"{"message":{"type":"answer","sdp":"test"}}"#;
        assert!(serde_json::from_str::<ServerToViewer>(json_no_type).is_err());
    }

    #[test]
    fn test_server_to_viewer_deserialize_ignores_unknown_fields() {
        let json = r#"{"type":"hostUnavailable","reason":"host disconnected"}"#;
        let deserialized: ServerToViewer = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized, ServerToViewer::HostUnavailable);
    }

    #[test]
    fn test_create_room_response_roundtrip() {
        let response = CreateRoomResponse {
            room_id: "room-abc-123".to_string(),
            host_token: "secret-token-456".to_string(),
        };

        let json =
            serde_json::to_string(&response).expect("failed to serialize CreateRoomResponse");
        let deserialized: CreateRoomResponse =
            serde_json::from_str(&json).expect("failed to deserialize CreateRoomResponse");

        assert_eq!(deserialized, response);
    }

    #[test]
    fn test_create_room_response_exact_json() {
        let response = CreateRoomResponse {
            room_id: "room-abc-123".to_string(),
            host_token: "secret-token-456".to_string(),
        };

        let json =
            serde_json::to_string(&response).expect("failed to serialize CreateRoomResponse");
        // Check the exact JSON with roomId and hostToken in field declaration order
        assert_eq!(
            json,
            r#"{"roomId":"room-abc-123","hostToken":"secret-token-456"}"#
        );

        let deserialized: CreateRoomResponse =
            serde_json::from_str(r#"{"roomId":"room-abc-123","hostToken":"secret-token-456"}"#)
                .expect("failed to deserialize exact JSON");
        assert_eq!(deserialized, response);
    }

    #[test]
    fn test_create_room_response_missing_host_token_fails() {
        // Missing required hostToken must fail deserialization
        let json_without_host_token = r#"{"roomId":"room-abc-123"}"#;
        let result = serde_json::from_str::<CreateRoomResponse>(json_without_host_token);

        assert!(result.is_err());
    }
}
