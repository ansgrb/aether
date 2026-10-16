use crate::connection::KernelConnectionInfo;
use crate::hmac::HmacSigner;
use crate::message::{
    ExecutionState, JupyterMessage, StreamName, WireError,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use zeromq::{PubSocket, RepSocket, RouterSocket, Socket, SocketRecv, SocketSend, ZmqError};

#[derive(Debug, Error)]
pub enum SocketLayerError {
    #[error("ZMQ socket error: {0}")]
    Zmq(#[from] ZmqError),
    #[error("wire protocol error: {0}")]
    Wire(#[from] WireError),
    #[error("IOPub channel closed")]
    IoPubClosed,
}

/// Cloneable handle for broadcasting messages to the `IOPub` (`PUB`) socket from any runtime.
#[derive(Clone)]
pub struct IoPubHandle {
    tx: mpsc::UnboundedSender<JupyterMessage>,
    signer: HmacSigner,
}

impl IoPubHandle {
    pub fn new(tx: mpsc::UnboundedSender<JupyterMessage>, signer: HmacSigner) -> Self {
        Self { tx, signer }
    }

    pub fn signer(&self) -> &HmacSigner {
        &self.signer
    }

    pub fn send_raw(&self, msg: JupyterMessage) -> Result<(), SocketLayerError> {
        self.tx
            .send(msg)
            .map_err(|_| SocketLayerError::IoPubClosed)
    }

    pub fn send_status(
        &self,
        state: ExecutionState,
        parent: Option<&JupyterMessage>,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_status(state, parent, &self.signer)?;
        self.send_raw(msg)
    }

    pub fn send_stream(
        &self,
        stream: StreamName,
        text: impl Into<String>,
        parent: Option<&JupyterMessage>,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_stream(stream, text, parent, &self.signer)?;
        self.send_raw(msg)
    }

    pub fn send_execute_input(
        &self,
        code: &str,
        execution_count: u64,
        parent: &JupyterMessage,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_execute_input(code, execution_count, parent, &self.signer)?;
        self.send_raw(msg)
    }

    pub fn send_execute_result(
        &self,
        execution_count: u64,
        data: HashMap<String, Value>,
        metadata: HashMap<String, Value>,
        parent: &JupyterMessage,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_execute_result(
            execution_count,
            data,
            metadata,
            parent,
            &self.signer,
        )?;
        self.send_raw(msg)
    }

    pub fn send_display_data(
        &self,
        data: HashMap<String, Value>,
        metadata: HashMap<String, Value>,
        parent: Option<&JupyterMessage>,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_display_data(data, metadata, parent, &self.signer)?;
        self.send_raw(msg)
    }

    pub fn send_error(
        &self,
        ename: &str,
        evalue: &str,
        traceback: &[String],
        parent: Option<&JupyterMessage>,
    ) -> Result<(), SocketLayerError> {
        let msg = JupyterMessage::iopub_error(ename, evalue, traceback, parent, &self.signer)?;
        self.send_raw(msg)
    }
}

/// Active ZeroMQ socket bundle for the 5 Jupyter channels.
pub struct KernelSockets {
    pub shell: Arc<Mutex<RouterSocket>>,
    pub control: Arc<Mutex<RouterSocket>>,
    pub stdin: Arc<Mutex<RouterSocket>>,
    pub iopub: IoPubHandle,
    pub signer: HmacSigner,
    pub hb_task: JoinHandle<()>,
    pub iopub_task: JoinHandle<()>,
}

impl KernelSockets {
    /// Bind all 5 ZeroMQ ports specified in `KernelConnectionInfo` and launch the Heartbeat
    /// and IOPub background tasks.
    pub async fn bind(info: &KernelConnectionInfo) -> Result<Self, SocketLayerError> {
        let signer = HmacSigner::new(info.key.as_bytes());

        // 1. Heartbeat (REP)
        let mut hb_socket = RepSocket::new();
        let hb_endpoint = info.hb_endpoint();
        hb_socket.bind(&hb_endpoint).await?;
        info!(endpoint = %hb_endpoint, "Bound Heartbeat (REP) socket");

        let hb_task = tokio::spawn(async move {
            loop {
                match hb_socket.recv().await {
                    Ok(ping) => {
                        if let Err(err) = hb_socket.send(ping).await {
                            warn!(error = %err, "Failed to send heartbeat reply");
                            break;
                        }
                    }
                    Err(err) => {
                        debug!(error = %err, "Heartbeat socket closed");
                        break;
                    }
                }
            }
        });

        // 2. IOPub (PUB)
        let mut iopub_socket = PubSocket::new();
        let iopub_endpoint = info.iopub_endpoint();
        iopub_socket.bind(&iopub_endpoint).await?;
        info!(endpoint = %iopub_endpoint, "Bound IOPub (PUB) socket");

        let (iopub_tx, mut iopub_rx) = mpsc::unbounded_channel::<JupyterMessage>();
        let iopub_task = tokio::spawn(async move {
            while let Some(msg) = iopub_rx.recv().await {
                let zmq_msg = msg.into_zmq_message(false);
                if let Err(err) = iopub_socket.send(zmq_msg).await {
                    error!(error = %err, "Failed to broadcast message on IOPub");
                }
            }
        });

        // 3. Shell (ROUTER)
        let mut shell_socket = RouterSocket::new();
        let shell_endpoint = info.shell_endpoint();
        shell_socket.bind(&shell_endpoint).await?;
        info!(endpoint = %shell_endpoint, "Bound Shell (ROUTER) socket");

        // 4. Control (ROUTER)
        let mut control_socket = RouterSocket::new();
        let control_endpoint = info.control_endpoint();
        control_socket.bind(&control_endpoint).await?;
        info!(endpoint = %control_endpoint, "Bound Control (ROUTER) socket");

        // 5. Stdin (ROUTER)
        let mut stdin_socket = RouterSocket::new();
        let stdin_endpoint = info.stdin_endpoint();
        stdin_socket.bind(&stdin_endpoint).await?;
        info!(endpoint = %stdin_endpoint, "Bound Stdin (ROUTER) socket");

        let iopub = IoPubHandle::new(iopub_tx, signer.clone());

        Ok(Self {
            shell: Arc::new(Mutex::new(shell_socket)),
            control: Arc::new(Mutex::new(control_socket)),
            stdin: Arc::new(Mutex::new(stdin_socket)),
            iopub,
            signer,
            hb_task,
            iopub_task,
        })
    }
}
