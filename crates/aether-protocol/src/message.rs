use crate::hmac::{HmacSigner, SignatureError};
use bytes::Bytes;
use chrono::Utc;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;
use zeromq::ZmqMessage;

pub const WIRE_DELIMITER: &str = "<IDS|MSG>";
pub const PROTOCOL_VERSION: &str = "5.3";

#[derive(Debug, Error)]
pub enum WireError {
    #[error("missing <IDS|MSG> delimiter in ZMQ multipart message")]
    MissingDelimiter,
    #[error("not enough frames after <IDS|MSG> delimiter (expected 5, found {0})")]
    InsufficientFrames(usize),
    #[error("invalid UTF-8 in message frame: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("HMAC verification failed: {0}")]
    Signature(#[from] SignatureError),
    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Raw Jupyter Wire Protocol message matching Section 2.2 of the specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JupyterMessage {
    pub zmq_identities: Vec<Vec<u8>>, // Frame 0..N before delimiter
    pub delimiter: String,            // "<IDS|MSG>"
    pub hmac_signature: String,       // Hex-encoded HMAC-SHA256
    pub header: String,               // Serialized JSON
    pub parent_header: String,        // Serialized JSON
    pub metadata: String,             // Serialized JSON
    pub content: String,              // Serialized JSON
    pub buffers: Vec<Vec<u8>>,        // Optional binary buffers
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageHeader {
    pub msg_id: String,
    pub session: String,
    pub username: String,
    pub date: String,
    pub msg_type: String,
    pub version: String,
}

impl MessageHeader {
    pub fn new(msg_type: impl Into<String>, session: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            msg_id: Uuid::new_v4().to_string(),
            session: session.into(),
            username: username.into(),
            date: Utc::now().to_rfc3339(),
            msg_type: msg_type.into(),
            version: PROTOCOL_VERSION.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub code: String,
    #[serde(default = "default_true")]
    pub silent: bool,
    #[serde(default = "default_true")]
    pub store_history: bool,
    #[serde(default)]
    pub user_expressions: HashMap<String, Value>,
    #[serde(default)]
    pub allow_stdin: bool,
    #[serde(default)]
    pub stop_on_error: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionState {
    Starting,
    Busy,
    Idle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamName {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownRequest {
    #[serde(default)]
    pub restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsCompleteRequest {
    pub code: String,
}

impl JupyterMessage {
    /// Parse and cryptographically verify a multipart ZMQ message.
    pub fn from_zmq_message(msg: ZmqMessage, signer: &HmacSigner) -> Result<Self, WireError> {
        let frames: Vec<Bytes> = msg.into_vec();
        let delim_pos = frames
            .iter()
            .position(|f| f.as_ref() == WIRE_DELIMITER.as_bytes())
            .ok_or(WireError::MissingDelimiter)?;

        let remaining = frames.len() - (delim_pos + 1);
        if remaining < 5 {
            return Err(WireError::InsufficientFrames(remaining));
        }

        let zmq_identities = frames[..delim_pos]
            .iter()
            .map(|b| b.to_vec())
            .collect::<Vec<_>>();
        let delimiter = String::from_utf8(frames[delim_pos].to_vec())?;
        let hmac_signature = String::from_utf8(frames[delim_pos + 1].to_vec())?;
        let header = String::from_utf8(frames[delim_pos + 2].to_vec())?;
        let parent_header = String::from_utf8(frames[delim_pos + 3].to_vec())?;
        let metadata = String::from_utf8(frames[delim_pos + 4].to_vec())?;
        let content = String::from_utf8(frames[delim_pos + 5].to_vec())?;
        let buffers = frames[delim_pos + 6..]
            .iter()
            .map(|b| b.to_vec())
            .collect::<Vec<_>>();

        signer.verify(&hmac_signature, &header, &parent_header, &metadata, &content)?;

        Ok(Self {
            zmq_identities,
            delimiter,
            hmac_signature,
            header,
            parent_header,
            metadata,
            content,
            buffers,
        })
    }

    /// Construct a signed response message tied to an optional parent message.
    pub fn new_signed<C: Serialize>(
        zmq_identities: Vec<Vec<u8>>,
        msg_type: &str,
        parent: Option<&JupyterMessage>,
        metadata_val: Option<Value>,
        content_val: &C,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        let (session, username, parent_header_str) = if let Some(p) = parent {
            let parsed_parent = p.parse_header()?;
            (
                parsed_parent.session,
                parsed_parent.username,
                p.header.clone(),
            )
        } else {
            (
                Uuid::new_v4().to_string(),
                "aether".to_string(),
                "{}".to_string(),
            )
        };

        let header_obj = MessageHeader::new(msg_type, session, username);
        let header = serde_json::to_string(&header_obj)?;
        let metadata = serde_json::to_string(&metadata_val.unwrap_or_else(|| json!({})))?;
        let content = serde_json::to_string(content_val)?;
        let hmac_signature = signer.sign(&header, &parent_header_str, &metadata, &content);

        Ok(Self {
            zmq_identities,
            delimiter: WIRE_DELIMITER.to_string(),
            hmac_signature,
            header,
            parent_header: parent_header_str,
            metadata,
            content,
            buffers: Vec::new(),
        })
    }

    /// Convert into a multipart `ZmqMessage` ready to send over a ZMQ socket.
    /// When `include_identities` is false (e.g. for `PUB` sockets on `IOPub`),
    /// the ZMQ routing prefix frames are omitted.
    pub fn into_zmq_message(self, include_identities: bool) -> ZmqMessage {
        let mut frames: Vec<Bytes> = Vec::new();
        if include_identities {
            for id in self.zmq_identities {
                frames.push(Bytes::from(id));
            }
        }
        frames.push(Bytes::from(self.delimiter.into_bytes()));
        frames.push(Bytes::from(self.hmac_signature.into_bytes()));
        frames.push(Bytes::from(self.header.into_bytes()));
        frames.push(Bytes::from(self.parent_header.into_bytes()));
        frames.push(Bytes::from(self.metadata.into_bytes()));
        frames.push(Bytes::from(self.content.into_bytes()));
        for buf in self.buffers {
            frames.push(Bytes::from(buf));
        }

        let mut iter = frames.into_iter();
        let first = iter.next().expect("at least 6 frames present");
        let mut msg = ZmqMessage::from(first);
        for frame in iter {
            msg.push_back(frame);
        }
        msg
    }

    pub fn parse_header(&self) -> Result<MessageHeader, WireError> {
        Ok(serde_json::from_str(&self.header)?)
    }

    pub fn parse_content<T: DeserializeOwned>(&self) -> Result<T, WireError> {
        Ok(serde_json::from_str(&self.content)?)
    }

    pub fn msg_type(&self) -> Result<String, WireError> {
        Ok(self.parse_header()?.msg_type)
    }

    /// Helper: construct an IOPub `status` message (`starting`, `busy`, `idle`).
    pub fn iopub_status(
        state: ExecutionState,
        parent: Option<&JupyterMessage>,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "status",
            parent,
            None,
            &json!({ "execution_state": state }),
            signer,
        )
    }

    /// Helper: construct an IOPub `stream` message (`stdout` or `stderr`).
    pub fn iopub_stream(
        stream: StreamName,
        text: impl Into<String>,
        parent: Option<&JupyterMessage>,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "stream",
            parent,
            None,
            &json!({
                "name": stream,
                "text": text.into(),
            }),
            signer,
        )
    }

    /// Helper: construct an IOPub `execute_input` message.
    pub fn iopub_execute_input(
        code: &str,
        execution_count: u64,
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "execute_input",
            Some(parent),
            None,
            &json!({
                "code": code,
                "execution_count": execution_count,
            }),
            signer,
        )
    }

    /// Helper: construct an IOPub `execute_result` message with a MIME bundle.
    pub fn iopub_execute_result(
        execution_count: u64,
        data: HashMap<String, Value>,
        metadata: HashMap<String, Value>,
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "execute_result",
            Some(parent),
            None,
            &json!({
                "execution_count": execution_count,
                "data": data,
                "metadata": metadata,
            }),
            signer,
        )
    }

    /// Helper: construct an IOPub `display_data` message with a MIME bundle.
    pub fn iopub_display_data(
        data: HashMap<String, Value>,
        metadata: HashMap<String, Value>,
        parent: Option<&JupyterMessage>,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "display_data",
            parent,
            None,
            &json!({
                "data": data,
                "metadata": metadata,
                "transient": {},
            }),
            signer,
        )
    }

    /// Helper: construct an IOPub `error` broadcast.
    pub fn iopub_error(
        ename: &str,
        evalue: &str,
        traceback: &[String],
        parent: Option<&JupyterMessage>,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            Vec::new(),
            "error",
            parent,
            None,
            &json!({
                "ename": ename,
                "evalue": evalue,
                "traceback": traceback,
            }),
            signer,
        )
    }

    /// Helper: construct a Shell `execute_reply` (`status: "ok"`).
    pub fn execute_reply_ok(
        execution_count: u64,
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            parent.zmq_identities.clone(),
            "execute_reply",
            Some(parent),
            None,
            &json!({
                "status": "ok",
                "execution_count": execution_count,
                "payload": [],
                "user_expressions": {},
            }),
            signer,
        )
    }

    /// Helper: construct a Shell `execute_reply` (`status: "error"`).
    pub fn execute_reply_error(
        execution_count: u64,
        ename: &str,
        evalue: &str,
        traceback: &[String],
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            parent.zmq_identities.clone(),
            "execute_reply",
            Some(parent),
            None,
            &json!({
                "status": "error",
                "execution_count": execution_count,
                "ename": ename,
                "evalue": evalue,
                "traceback": traceback,
            }),
            signer,
        )
    }

    /// Helper: construct a Shell `kernel_info_reply` message required by VS Code / JupyterLab.
    pub fn kernel_info_reply(
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            parent.zmq_identities.clone(),
            "kernel_info_reply",
            Some(parent),
            None,
            &json!({
                "status": "ok",
                "protocol_version": PROTOCOL_VERSION,
                "implementation": "aether",
                "implementation_version": env!("CARGO_PKG_VERSION"),
                "language_info": {
                    "name": "rust",
                    "version": "1.99.0",
                    "mimetype": "text/rust",
                    "file_extension": ".rs",
                    "pygments_lexer": "rust",
                    "codemirror_mode": "rust"
                },
                "banner": "Aether (RustyNote) — Polyglot Zero-Copy Rust & Python Kernel",
                "help_links": []
            }),
            signer,
        )
    }

    /// Helper: construct a `shutdown_reply`.
    pub fn shutdown_reply(
        restart: bool,
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            parent.zmq_identities.clone(),
            "shutdown_reply",
            Some(parent),
            None,
            &json!({
                "status": "ok",
                "restart": restart,
            }),
            signer,
        )
    }

    /// Helper: construct an `is_complete_reply`.
    pub fn is_complete_reply(
        status: &str,
        parent: &JupyterMessage,
        signer: &HmacSigner,
    ) -> Result<Self, WireError> {
        Self::new_signed(
            parent.zmq_identities.clone(),
            "is_complete_reply",
            Some(parent),
            None,
            &json!({
                "status": status,
            }),
            signer,
        )
    }
}
