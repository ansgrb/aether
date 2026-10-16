use aether_bridge::arrow_array::cast::AsArray;
use aether_bridge::arrow_array::types::Int64Type;
use aether_bridge::load_shared_record_batch;
use aether_engine_python::PythonEngine;
use aether_protocol::{
    HmacSigner, JupyterMessage, KernelConnectionInfo, KernelSockets,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;
use zeromq::{DealerSocket, ReqSocket, Socket, SocketRecv, SocketSend, SubSocket};

/// Sprint 1 Verification: Full ZMQ Socket Backbone, Heartbeat Echo, and HMAC-Signed Wire Framing.
#[tokio::test]
async fn test_sprint1_zmq_backbone_and_hmac_protocol() {
    let info = KernelConnectionInfo {
        ip: "127.0.0.1".to_string(),
        transport: "tcp".to_string(),
        shell_port: 55101,
        iopub_port: 55102,
        stdin_port: 55103,
        control_port: 55104,
        hb_port: 55105,
        key: "aether-sprint1-secret-key".to_string(),
        signature_scheme: "hmac-sha256".to_string(),
        kernel_name: Some("aether".to_string()),
    };

    let sockets = KernelSockets::bind(&info)
        .await
        .expect("should bind all 5 ZMQ sockets");
    let signer = HmacSigner::new(info.key.as_bytes());

    // 1. Verify Heartbeat (REQ/REP)
    let mut hb_client = ReqSocket::new();
    hb_client
        .connect(&info.hb_endpoint())
        .await
        .expect("connect to HB");
    hb_client
        .send("ping-aether".into())
        .await
        .expect("send ping");
    let pong = hb_client.recv().await.expect("recv pong");
    assert_eq!(pong.get(0).unwrap().as_ref(), b"ping-aether");

    // 2. Verify IOPub (PUB/SUB) and Shell (ROUTER/DEALER) with HMAC-SHA256
    let mut sub_client = SubSocket::new();
    sub_client
        .connect(&info.iopub_endpoint())
        .await
        .expect("connect to IOPub");
    sub_client.subscribe("").await.expect("subscribe all");

    let mut shell_client = DealerSocket::new();
    shell_client
        .connect(&info.shell_endpoint())
        .await
        .expect("connect to Shell");

    // Send kernel_info_request from client
    let req_msg = JupyterMessage::new_signed(
        Vec::new(),
        "kernel_info_request",
        None,
        None,
        &serde_json::json!({}),
        &signer,
    )
    .unwrap();
    shell_client
        .send(req_msg.into_zmq_message(false))
        .await
        .unwrap();

    // Receive on server Shell ROUTER socket, verify HMAC, and send kernel_info_reply
    let raw_incoming = {
        let mut shell_srv = sockets.shell.lock().await;
        shell_srv.recv().await.unwrap()
    };
    let parsed_req =
        JupyterMessage::from_zmq_message(raw_incoming, &signer).expect("HMAC should verify");
    assert_eq!(parsed_req.msg_type().unwrap(), "kernel_info_request");

    let reply = JupyterMessage::kernel_info_reply(&parsed_req, &signer).unwrap();
    {
        let mut shell_srv = sockets.shell.lock().await;
        shell_srv
            .send(reply.into_zmq_message(true))
            .await
            .unwrap();
    }

    let raw_reply = shell_client.recv().await.unwrap();
    let parsed_reply =
        JupyterMessage::from_zmq_message(raw_reply, &signer).expect("client verifies reply HMAC");
    assert_eq!(parsed_reply.msg_type().unwrap(), "kernel_info_reply");
    let content: Value = parsed_reply.parse_content().unwrap();
    assert_eq!(content["implementation"], "aether");

    sockets.hb_task.abort();
    sockets.iopub_task.abort();
}

/// Sprint 4 Verification: 1 GB DataFrame created in `%%python` and queried in Rust in < 5ms (Zero-Copy).
#[test]
fn test_sprint4_one_gigabyte_arrow_zero_copy_under_5ms() {
    let py_engine = PythonEngine::new().expect("init PythonEngine");
    let env = HashMap::new();

    // Create a 1 GB (134,217,728 x 8-byte int64 = 1,073,741,824 bytes = 1.0 GiB) Arrow/Polars table in Python
    // and share it via `__share_to_rust__("one_gb_df", df)`.
    let setup_code = r#"
import pyarrow as pa
import polars as pl

# Allocate 1 GiB contiguous buffer (134,217,728 int64 elements)
num_rows = 134_217_728
buf = bytearray(num_rows * 8)
# Write sentinel values at the first and last int64 positions
import struct
struct.pack_into("<q", buf, 0, 42)
struct.pack_into("<q", buf, (num_rows - 1) * 8, 99)

arr = pa.Array.from_buffers(pa.int64(), num_rows, [None, pa.py_buffer(buf)])
rb = pa.RecordBatch.from_arrays([arr], names=["val"])
df = pl.from_arrow(rb)
__share_to_rust__("one_gb_df", df)
"#;

    py_engine
        .execute_cell(setup_code, &env, None, None)
        .expect("1 GiB dataframe creation and sharing in Python should succeed");

    // Measure the time to reconstruct the 1 GiB RecordBatch in Rust via the Arrow C-Data Interface
    // (`FFI_ArrowArray` and `FFI_ArrowSchema` pointers) and query it!
    let start = Instant::now();
    let batch =
        load_shared_record_batch("one_gb_df").expect("should zero-copy load 1 GiB RecordBatch");
    let col = batch.column(0).as_primitive::<Int64Type>();
    let first = col.value(0);
    let last = col.value(col.len() - 1);
    let elapsed = start.elapsed();

    println!(
        "1 GiB Arrow DataFrame Python -> Rust zero-copy handoff + query took: {:?}",
        elapsed
    );

    assert_eq!(batch.num_rows(), 134_217_728);
    assert_eq!(col.values().len() * std::mem::size_of::<i64>(), 1_073_741_824);
    assert_eq!(first, 42);
    assert_eq!(last, 99);
    assert!(
        elapsed.as_millis() < 5,
        "Expected zero-copy query latency < 5ms, but took {:?}",
        elapsed
    );
}

/// End-to-End Verification across Sprints 1–5:
/// Spawns the `rust-notebook-core` binary (which runs `evcxr::runtime_hook()` at top of `main()`),
/// connects via ZeroMQ, and executes:
/// 1. `kernel_info_request`
/// 2. `%%env` cell (TOML config propagating to both Python and Rust)
/// 3. `%%python` cell creating a Polars DataFrame and calling `__share_to_rust__("my_df", df)`
/// 4. `%%rust` cell compiled by `evcxr` reading the shared `my_df` via zero-copy `mmap` AND reading the `%%env` var
/// 5. `%%rust` cell returning an `Svg(...)` rich display payload
/// 6. `shutdown_request`
#[tokio::test]
async fn test_end_to_end_kernel_binary_polyglot_execution() {
    let bin_path = env!("CARGO_BIN_EXE_rust-notebook-core");
    let conn_info = KernelConnectionInfo {
        ip: "127.0.0.1".to_string(),
        transport: "tcp".to_string(),
        shell_port: 55211,
        iopub_port: 55212,
        stdin_port: 55213,
        control_port: 55214,
        hb_port: 55215,
        key: "e2e-polyglot-hmac-key".to_string(),
        signature_scheme: "hmac-sha256".to_string(),
        kernel_name: Some("aether".to_string()),
    };

    let tmp_conn_file = std::env::temp_dir().join(format!(
        "aether_e2e_kernel_{}.json",
        std::process::id()
    ));
    std::fs::write(
        &tmp_conn_file,
        serde_json::to_string_pretty(&conn_info).unwrap(),
    )
    .unwrap();

    let mut child = std::process::Command::new(bin_path)
        .arg("-f")
        .arg(&tmp_conn_file)
        .spawn()
        .expect("failed to spawn rust-notebook-core binary");

    let signer = HmacSigner::new(conn_info.key.as_bytes());

    // Wait briefly for the kernel process to bind ZMQ ports and initialize evcxr
    let mut hb_client = ReqSocket::new();
    hb_client.connect(&conn_info.hb_endpoint()).await.unwrap();
    hb_client.send("ready?".into()).await.unwrap();
    let pong = hb_client.recv().await.unwrap();
    assert_eq!(pong.get(0).unwrap().as_ref(), b"ready?");

    let mut shell = DealerSocket::new();
    shell.connect(&conn_info.shell_endpoint()).await.unwrap();

    let mut iopub = SubSocket::new();
    iopub.connect(&conn_info.iopub_endpoint()).await.unwrap();
    iopub.subscribe("").await.unwrap();

    // Helper closure to send an execute_request and wait for execute_reply
    async fn run_cell(
        shell: &mut DealerSocket,
        signer: &HmacSigner,
        code: &str,
    ) -> JupyterMessage {
        let req = JupyterMessage::new_signed(
            Vec::new(),
            "execute_request",
            None,
            None,
            &serde_json::json!({
                "code": code,
                "silent": false,
                "store_history": true,
                "user_expressions": {},
                "allow_stdin": false,
                "stop_on_error": true
            }),
            signer,
        )
        .unwrap();
        shell.send(req.into_zmq_message(false)).await.unwrap();
        let raw_reply = shell.recv().await.unwrap();
        JupyterMessage::from_zmq_message(raw_reply, signer).unwrap()
    }

    // Step 1: %%env cell
    let reply_env = run_cell(
        &mut shell,
        &signer,
        "%%env\nDB_HOST = \"db.aether.internal\"\nAPI_SECRET = \"topsecret\"",
    )
    .await;
    let env_content: Value = reply_env.parse_content().unwrap();
    assert_eq!(env_content["status"], "ok");

    // Step 2: %%python cell reading env var and sharing Polars DataFrame to Rust
    let reply_py = run_cell(
        &mut shell,
        &signer,
        r#"%%python
import os, polars as pl
assert os.environ["DB_HOST"] == "db.aether.internal"
df = pl.DataFrame({"nums": [10, 20, 30, 40]})
__share_to_rust__("my_df", df)
int(df["nums"].sum())
"#,
    )
    .await;
    let py_content: Value = reply_py.parse_content().unwrap();
    assert_eq!(
        py_content["status"], "ok",
        "Python cell failed: {:?}",
        py_content
    );

    // Step 3: %%rust cell compiled via evcxr consuming `my_df` zero-copy and checking env var
    let reply_rs = run_cell(
        &mut shell,
        &signer,
        r#"%%rust
let host = std::env::var("DB_HOST").unwrap();
let sum: i64 = my_df.col_i64("nums").iter().sum();
format!("{}:{}", host, sum)
"#,
    )
    .await;
    let rs_content: Value = reply_rs.parse_content().unwrap();
    assert_eq!(
        rs_content["status"], "ok",
        "Rust cell failed: {:?}",
        rs_content
    );

    // Step 4: %%rust cell returning a rich `Svg` display object (Sprint 5)
    let reply_svg = run_cell(
        &mut shell,
        &signer,
        r#"%%rust
Svg("<svg width=\"50\" height=\"50\"><rect width=\"50\" height=\"50\"/></svg>")
"#,
    )
    .await;
    let svg_content: Value = reply_svg.parse_content().unwrap();
    assert_eq!(
        svg_content["status"], "ok",
        "Rust SVG cell failed: {:?}",
        svg_content
    );

    // Step 5: Shutdown kernel cleanly
    let shutdown_req = JupyterMessage::new_signed(
        Vec::new(),
        "shutdown_request",
        None,
        None,
        &serde_json::json!({ "restart": false }),
        &signer,
    )
    .unwrap();
    shell
        .send(shutdown_req.into_zmq_message(false))
        .await
        .unwrap();
    let raw_shutdown_reply = shell.recv().await.unwrap();
    let parsed_shutdown =
        JupyterMessage::from_zmq_message(raw_shutdown_reply, &signer).unwrap();
    assert_eq!(parsed_shutdown.msg_type().unwrap(), "shutdown_reply");

    let status = child.wait().expect("failed to wait on kernel process");
    assert!(status.success());
    let _ = std::fs::remove_file(&tmp_conn_file);
}

