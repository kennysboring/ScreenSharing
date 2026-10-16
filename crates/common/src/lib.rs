use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "type")]
pub enum SignalMessage {
    Offer { 
        sdp: String 
    },
    Answer { 
        sdp: String 
    },
    Ice { 
        candidate: String,

        #[serde(rename = "sdpMid", skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,

        #[serde(rename = "sdpMLineIndex", skip_serializing_if = "Option::is_none")]
        sdp_mline_index: Option<u16>,

        #[serde(rename = "usernameFragment", skip_serializing_if = "Option::is_none")]
        username_fragment: Option<String>, 
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
pub struct ViewerId(pub u32);

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "type")]
pub enum ServerMessage {
    PeerJoined { id: ViewerId },
    PeerLeft { id: ViewerId },
}





#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offer_serialization_and_deserialization() {
        let msg = SignalMessage::Offer {
            sdp: "teste".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"Offer","sdp":"teste"}"#);

        let deserialized: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_answer_serialization_and_deserialization() {
        let msg = SignalMessage::Answer {
            sdp: "teste".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"Answer","sdp":"teste"}"#);

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
            r#"{"type":"Ice","candidate":"candidate:1","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"ufrag123"}"#
        );

        let deserialized: SignalMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_ice_optional_fields_as_explicit_null() {
        // O navegador web frequentemente envia "sdpMid": null ou "usernameFragment": null
        let json = r#"{
            "type": "Ice",
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

        // Option desserializa JSON `null` como `None`
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_ice_optional_fields_omitted() {
        // Campos ausentes no JSON também são desserializados como `None`
        let json = r#"{
            "type": "Ice",
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
        // Quando campos são None, skip_serializing_if faz com que não apareçam no JSON gerado
        let msg = SignalMessage::Ice {
            candidate: "candidate:1".to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
            username_fragment: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"Ice","candidate":"candidate:1"}"#);
    }

    #[test]
    fn test_deserialize_invalid_type() {
        let json = r#"{"type":"Unknown","sdp":"..."}"#;
        let result = serde_json::from_str::<SignalMessage>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_missing_required_field() {
        // Falta campo obrigatório `sdp` em Offer
        let json_offer_no_sdp = r#"{"type":"Offer"}"#;
        let result_offer = serde_json::from_str::<SignalMessage>(json_offer_no_sdp);
        assert!(result_offer.is_err());

        // Falta campo obrigatório `candidate` em Ice
        let json_ice_no_candidate = r#"{"type":"Ice"}"#;
        let result_ice = serde_json::from_str::<SignalMessage>(json_ice_no_candidate);
        assert!(result_ice.is_err());
    }

    #[test]
    fn test_deserialize_ignores_unknown_fields() {
        // O Serde ignora campos não mapeados por padrão (decisão de protocolo tolerante à evolução)
        let json = r#"{"type":"Offer","sdp":"teste","novo":1,"extra":{"nested":true}}"#;
        let deserialized = serde_json::from_str::<SignalMessage>(json);
        assert_eq!(
            deserialized.unwrap(),
            SignalMessage::Offer {
                sdp: "teste".to_string()
            }
        );
    }

    #[test]
    fn test_viewer_id_transparent_serialization_and_deserialization() {
        let viewer_id = ViewerId(42);
        // Newtype struct serializa diretamente como o número `42`
        let json = serde_json::to_string(&viewer_id).unwrap();
        assert_eq!(json, "42");

        let deserialized: ViewerId = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, viewer_id);
    }

    #[test]
    fn test_viewer_id_boundaries_and_invalid_deserialization() {
        // Valores válidos no limite de u32
        assert_eq!(serde_json::from_str::<ViewerId>("0").unwrap(), ViewerId(0));
        assert_eq!(
            serde_json::from_str::<ViewerId>("4294967295").unwrap(),
            ViewerId(u32::MAX)
        );

        // String em vez de inteiro deve falhar
        assert!(serde_json::from_str::<ViewerId>(r#""42""#).is_err());
        // Overflow de u32 (> 4_294_967_295) deve falhar
        assert!(serde_json::from_str::<ViewerId>("4294967296").is_err());
        // Número negativo deve falhar para tipo unsigned
        assert!(serde_json::from_str::<ViewerId>("-1").is_err());
    }

    #[test]
    fn test_server_message_peer_joined_serialization_and_deserialization() {
        let msg = ServerMessage::PeerJoined {
            id: ViewerId(7),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"PeerJoined","id":7}"#);

        let deserialized: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_message_peer_left_serialization_and_deserialization() {
        let msg = ServerMessage::PeerLeft {
            id: ViewerId(7),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"PeerLeft","id":7}"#);

        let deserialized: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    fn test_server_message_deserialize_invalid_type() {
        let json = r#"{"type":"UnknownPeer","id":1}"#;
        assert!(serde_json::from_str::<ServerMessage>(json).is_err());
    }

    #[test]
    fn test_server_message_deserialize_missing_fields() {
        // Sem id obrigatório
        let json_no_id = r#"{"type":"PeerJoined"}"#;
        assert!(serde_json::from_str::<ServerMessage>(json_no_id).is_err());

        // Sem a tag type
        let json_no_type = r#"{"id":1}"#;
        assert!(serde_json::from_str::<ServerMessage>(json_no_type).is_err());
    }

    #[test]
    fn test_server_message_deserialize_ignores_unknown_fields() {
        let json = r#"{"type":"PeerJoined","id":10,"metadata":"extra"}"#;
        let deserialized: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            ServerMessage::PeerJoined {
                id: ViewerId(10)
            }
        );
    }
}
