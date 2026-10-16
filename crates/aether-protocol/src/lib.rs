pub mod connection;
pub mod hmac;
pub mod message;
pub mod sockets;

pub use connection::{ConnectionError, KernelConnectionInfo};
pub use hmac::{HmacSigner, SignatureError};
pub use message::{
    ExecuteRequest, ExecutionState, IsCompleteRequest, JupyterMessage, MessageHeader,
    ShutdownRequest, StreamName, WireError, PROTOCOL_VERSION, WIRE_DELIMITER,
};
pub use sockets::{IoPubHandle, KernelSockets, SocketLayerError};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hmac_sign_and_verify_roundtrip() {
        let signer = HmacSigner::new(b"super-secret-kernel-key");
        let header = r#"{"msg_id":"1","session":"s","username":"u","date":"2026-10-16T00:00:00Z","msg_type":"execute_request","version":"5.3"}"#;
        let parent = "{}";
        let metadata = "{}";
        let content = r#"{"code":"1 + 1"}"#;

        let sig = signer.sign(header, parent, metadata, content);
        assert!(!sig.is_empty());
        assert_eq!(
            signer.verify(&sig, header, parent, metadata, content),
            Ok(())
        );

        // Tamper with content
        assert_eq!(
            signer.verify(&sig, header, parent, metadata, r#"{"code":"rm -rf /"}"#),
            Err(SignatureError::VerificationFailed)
        );
    }

    #[test]
    fn test_jupyter_message_zmq_roundtrip() {
        let signer = HmacSigner::new(b"test-key-123");
        let msg = JupyterMessage::new_signed(
            vec![b"client-identity-1".to_vec()],
            "execute_request",
            None,
            None,
            &json!({ "code": "let x = 42;" }),
            &signer,
        )
        .expect("should build signed message");

        let zmq_msg = msg.clone().into_zmq_message(true);
        let decoded = JupyterMessage::from_zmq_message(zmq_msg, &signer)
            .expect("should verify and parse ZMQ message");

        assert_eq!(decoded.zmq_identities, vec![b"client-identity-1".to_vec()]);
        assert_eq!(decoded.delimiter, WIRE_DELIMITER);
        assert_eq!(decoded.msg_type().unwrap(), "execute_request");
        let req: ExecuteRequest = decoded.parse_content().unwrap();
        assert_eq!(req.code, "let x = 42;");
    }
}
