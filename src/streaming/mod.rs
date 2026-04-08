pub mod spdy;

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Body;
use hyper::header::{CONNECTION, UPGRADE};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Method, Request, Response, Server, StatusCode};
use nix::pty::openpty;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;

use crate::attach::{AttachOutputDecoder, ATTACH_PIPE_STDERR, ATTACH_PIPE_STDOUT};
use crate::proto::runtime::v1::{AttachRequest, AttachResponse, ExecRequest, ExecResponse};

#[derive(Debug, Clone)]
pub enum StreamingRequest {
    Exec(ExecRequest),
    Attach(AttachRequest),
}

#[derive(Debug, Clone)]
pub struct StreamingServer {
    cache: Arc<Mutex<HashMap<String, StreamingRequest>>>,
    base_url: String,
}

impl StreamingServer {
    #[cfg(test)]
    fn for_test(base_url: impl Into<String>) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            base_url: base_url.into(),
        }
    }

    pub async fn start(bind_addr: &str, runtime_path: PathBuf) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(bind_addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let service_cache = cache.clone();
        let service_runtime_path = runtime_path.clone();

        let make_service = make_service_fn(move |_| {
            let cache = service_cache.clone();
            let runtime_path = service_runtime_path.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    let cache = cache.clone();
                    let runtime_path = runtime_path.clone();
                    async move { Ok::<_, Infallible>(handle_request(cache, runtime_path, req).await) }
                }))
            }
        });

        let server = Server::from_tcp(listener)?.serve(make_service);
        tokio::spawn(async move {
            if let Err(e) = server.await {
                log::error!("Streaming server exited: {}", e);
            }
        });

        Ok(Self {
            cache,
            base_url: format!("http://{}", local_addr),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn get_exec(&self, req: &ExecRequest) -> Result<ExecResponse, tonic::Status> {
        Self::validate_exec_request(req)?;
        let token = self
            .insert_request(StreamingRequest::Exec(req.clone()))
            .await;
        Ok(ExecResponse {
            url: format!("{}/exec/{}", self.base_url, token),
        })
    }

    pub async fn get_attach(&self, req: &AttachRequest) -> Result<AttachResponse, tonic::Status> {
        Self::validate_attach_request(req)?;
        let token = self
            .insert_request(StreamingRequest::Attach(req.clone()))
            .await;
        Ok(AttachResponse {
            url: format!("{}/attach/{}", self.base_url, token),
        })
    }

    async fn insert_request(&self, request: StreamingRequest) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let mut cache = self.cache.lock().await;
        cache.insert(token.clone(), request);
        token
    }

    fn validate_exec_request(req: &ExecRequest) -> Result<(), tonic::Status> {
        if req.container_id.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "missing required container_id",
            ));
        }
        if req.tty && req.stderr {
            return Err(tonic::Status::invalid_argument(
                "tty and stderr cannot both be true",
            ));
        }
        if req.cmd.is_empty() {
            return Err(tonic::Status::invalid_argument("cmd must not be empty"));
        }
        if !req.stdin && !req.stdout && !req.stderr {
            return Err(tonic::Status::invalid_argument(
                "one of stdin, stdout, or stderr must be set",
            ));
        }
        Ok(())
    }

    fn validate_attach_request(req: &AttachRequest) -> Result<(), tonic::Status> {
        if req.container_id.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "missing required container_id",
            ));
        }
        if req.tty && req.stderr {
            return Err(tonic::Status::invalid_argument(
                "tty and stderr cannot both be true",
            ));
        }
        if !req.stdin && !req.stdout && !req.stderr {
            return Err(tonic::Status::invalid_argument(
                "one of stdin, stdout, or stderr must be set",
            ));
        }
        Ok(())
    }
}

async fn handle_request(
    cache: Arc<Mutex<HashMap<String, StreamingRequest>>>,
    runtime_path: PathBuf,
    req: Request<Body>,
) -> Response<Body> {
    if req.method() != Method::GET && req.method() != Method::POST {
        return response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }

    let path = req.uri().path().trim_matches('/');
    let mut parts = path.split('/');
    let action = match parts.next() {
        Some(value) if !value.is_empty() => value,
        _ => return response(StatusCode::NOT_FOUND, "streaming token not found"),
    };
    let token = match parts.next() {
        Some(value) if !value.is_empty() => value,
        _ => return response(StatusCode::NOT_FOUND, "streaming token not found"),
    };

    let request = {
        let mut cache = cache.lock().await;
        cache.remove(token)
    };

    match (action, request) {
        ("exec", Some(StreamingRequest::Exec(exec_req))) => {
            if !is_spdy_upgrade_request(&req) {
                return response(
                    StatusCode::BAD_REQUEST,
                    "exec requires SPDY upgrade headers",
                );
            }

            let Some(protocol) = negotiate_remotecommand_protocol(&req) else {
                return response(
                    StatusCode::FORBIDDEN,
                    "no supported X-Stream-Protocol-Version was requested",
                );
            };

            let on_upgrade = hyper::upgrade::on(req);
            tokio::spawn(async move {
                if let Err(e) = serve_exec_spdy(on_upgrade, exec_req, runtime_path, protocol).await
                {
                    log::error!("Exec SPDY session failed: {}", e);
                }
            });

            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(CONNECTION, "Upgrade")
                .header(UPGRADE, spdy::SPDY_31)
                .header("X-Stream-Protocol-Version", protocol)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
        ("attach", Some(StreamingRequest::Attach(attach_req))) => {
            if !is_spdy_upgrade_request(&req) {
                return response(
                    StatusCode::BAD_REQUEST,
                    "attach requires SPDY upgrade headers",
                );
            }

            let Some(protocol) = negotiate_remotecommand_protocol(&req) else {
                return response(
                    StatusCode::FORBIDDEN,
                    "no supported X-Stream-Protocol-Version was requested",
                );
            };

            let on_upgrade = hyper::upgrade::on(req);
            tokio::spawn(async move {
                if let Err(e) = serve_attach_spdy(on_upgrade, attach_req, protocol).await {
                    log::error!("Attach SPDY session failed: {}", e);
                }
            });

            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(CONNECTION, "Upgrade")
                .header(UPGRADE, spdy::SPDY_31)
                .header("X-Stream-Protocol-Version", protocol)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
        ("exec", Some(_)) | ("attach", Some(_)) => {
            response(StatusCode::BAD_REQUEST, "streaming token kind mismatch")
        }
        _ => response(StatusCode::NOT_FOUND, "streaming token not found"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachStreamRole {
    Error,
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecStreamRole {
    Error,
    Stdin,
    Stdout,
    Stderr,
}

enum ExecConsoleEvent {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

fn expected_attach_roles(req: &AttachRequest) -> Vec<AttachStreamRole> {
    let mut roles = vec![AttachStreamRole::Error];
    if req.stdin {
        roles.push(AttachStreamRole::Stdin);
    }
    if req.stdout {
        roles.push(AttachStreamRole::Stdout);
    }
    if req.stderr && !req.tty {
        roles.push(AttachStreamRole::Stderr);
    }
    roles
}

fn expected_exec_roles(req: &ExecRequest) -> Vec<ExecStreamRole> {
    let mut roles = vec![ExecStreamRole::Error];
    if req.stdin {
        roles.push(ExecStreamRole::Stdin);
    }
    if req.stdout {
        roles.push(ExecStreamRole::Stdout);
    }
    if req.stderr && !req.tty {
        roles.push(ExecStreamRole::Stderr);
    }
    roles
}

fn is_spdy_upgrade_request(req: &Request<Body>) -> bool {
    let connection = req
        .headers()
        .get(CONNECTION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let upgrade = req
        .headers()
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    connection.contains("upgrade") && upgrade.contains("spdy/3.1")
}

fn negotiate_remotecommand_protocol(req: &Request<Body>) -> Option<&'static str> {
    let requested: Vec<String> = req
        .headers()
        .get_all("X-Stream-Protocol-Version")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();

    if requested
        .iter()
        .any(|value| value == spdy::STREAM_PROTOCOL_V2)
    {
        return Some(spdy::STREAM_PROTOCOL_V2);
    }
    if requested.iter().any(|value| value == "channel.k8s.io") {
        return Some("channel.k8s.io");
    }
    None
}

async fn serve_exec_spdy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    req: ExecRequest,
    runtime_path: PathBuf,
    _protocol: &'static str,
) -> anyhow::Result<()> {
    let upgraded = on_upgrade.await?;
    let (read_half, write_half) = tokio::io::split(upgraded);
    let writer = Arc::new(Mutex::new(spdy::AsyncSpdyWriter::new(write_half)));
    let mut reader = read_half;

    let expected_roles = expected_exec_roles(&req);
    let mut header_decompressor = spdy::HeaderDecompressor::new();
    let mut stdin_stream = None;
    let mut stdout_stream = None;
    let mut stderr_stream = None;
    let mut error_stream = None;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while [error_stream, stdin_stream, stdout_stream, stderr_stream]
        .into_iter()
        .flatten()
        .count()
        < expected_roles.len()
    {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, spdy::read_frame_async(&mut reader)).await??;
        match frame {
            spdy::Frame::SynStream(frame) => {
                let headers =
                    spdy::decode_header_block(&frame.header_block, &mut header_decompressor)?;
                let stream_type = spdy::header_value(&headers, "streamtype")
                    .ok_or_else(|| anyhow::anyhow!("exec stream is missing streamtype header"))?;
                let role = match stream_type {
                    "error" => ExecStreamRole::Error,
                    "stdin" => ExecStreamRole::Stdin,
                    "stdout" => ExecStreamRole::Stdout,
                    "stderr" => ExecStreamRole::Stderr,
                    other => return Err(anyhow::anyhow!("unsupported exec streamtype {}", other)),
                };

                if !expected_roles.contains(&role) {
                    return Err(anyhow::anyhow!(
                        "unexpected exec stream {:?} for request",
                        role
                    ));
                }

                match role {
                    ExecStreamRole::Error if error_stream.is_none() => {
                        error_stream = Some(frame.stream_id)
                    }
                    ExecStreamRole::Stdin if stdin_stream.is_none() => {
                        stdin_stream = Some(frame.stream_id)
                    }
                    ExecStreamRole::Stdout if stdout_stream.is_none() => {
                        stdout_stream = Some(frame.stream_id)
                    }
                    ExecStreamRole::Stderr if stderr_stream.is_none() => {
                        stderr_stream = Some(frame.stream_id)
                    }
                    _ => {
                        return Err(anyhow::anyhow!("duplicate exec stream {:?} received", role));
                    }
                }

                writer
                    .lock()
                    .await
                    .write_syn_reply(frame.stream_id, &[], false)
                    .await?;
            }
            spdy::Frame::Ping(frame) => {
                writer.lock().await.write_ping(frame.id).await?;
            }
            _ => {}
        }
    }

    let mut command = TokioCommand::new(&runtime_path);
    command.arg("exec");
    if req.tty {
        command.arg("-t");
    }
    command.arg(&req.container_id);
    for arg in &req.cmd {
        command.arg(arg);
    }

    let mut tty_master = None;
    if req.tty {
        let pty = openpty(None, None)?;
        let master = unsafe { File::from_raw_fd(pty.master) };
        let slave = unsafe { File::from_raw_fd(pty.slave) };
        let slave_stdin = slave.try_clone()?;
        let slave_stdout = slave.try_clone()?;
        let slave_stderr = slave;
        let slave_fd = slave_stderr.as_raw_fd();

        command.stdin(Stdio::from(slave_stdin));
        command.stdout(Stdio::from(slave_stdout));
        command.stderr(Stdio::from(slave_stderr));
        unsafe {
            command.pre_exec(move || {
                if nix::unistd::setsid().is_err() {
                    return Err(std::io::Error::last_os_error());
                }
                if nix::libc::ioctl(slave_fd, nix::libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        tty_master = Some(master);
    } else {
        command.stdin(if req.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(if req.stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stderr(if req.stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            if let Some(stream_id) = error_stream {
                let mut writer = writer.lock().await;
                let _ = writer
                    .write_data(
                        stream_id,
                        format!("failed to spawn exec process: {}", e).as_bytes(),
                        false,
                    )
                    .await;
                let _ = writer.write_data(stream_id, &[], true).await;
                let _ = writer.write_goaway(stream_id).await;
            }
            return Err(e.into());
        }
    };

    let stdout_task;
    let stderr_task;
    let mut console_input_task = None;
    let mut console_stdin_tx = None;

    if req.tty {
        let master = tty_master
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing exec tty master"))?;
        let master_reader = master.try_clone()?;
        let master_writer = master;
        let (console_out_tx, mut console_out_rx) =
            tokio::sync::mpsc::channel::<ExecConsoleEvent>(8);

        tokio::task::spawn_blocking(move || {
            let mut reader = master_reader;
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = console_out_tx.blocking_send(ExecConsoleEvent::Eof);
                        break;
                    }
                    Ok(n) => {
                        if console_out_tx
                            .blocking_send(ExecConsoleEvent::Data(buffer[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ =
                            console_out_tx.blocking_send(ExecConsoleEvent::Error(e.to_string()));
                        break;
                    }
                }
            }
        });

        let writer_for_console_output = writer.clone();
        stdout_task = Some(tokio::spawn(async move {
            while let Some(event) = console_out_rx.recv().await {
                match event {
                    ExecConsoleEvent::Data(data) => {
                        if let Some(stream_id) = stdout_stream {
                            writer_for_console_output
                                .lock()
                                .await
                                .write_data(stream_id, &data, false)
                                .await?;
                        }
                    }
                    ExecConsoleEvent::Eof => {
                        if let Some(stream_id) = stdout_stream {
                            writer_for_console_output
                                .lock()
                                .await
                                .write_data(stream_id, &[], true)
                                .await?;
                        }
                        break;
                    }
                    ExecConsoleEvent::Error(message) => {
                        if let Some(stream_id) = error_stream {
                            writer_for_console_output
                                .lock()
                                .await
                                .write_data(stream_id, message.as_bytes(), false)
                                .await?;
                        }
                        if let Some(stream_id) = stdout_stream {
                            writer_for_console_output
                                .lock()
                                .await
                                .write_data(stream_id, &[], true)
                                .await?;
                        }
                        break;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }));
        stderr_task = None;

        if req.stdin {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<Vec<u8>>>(8);
            console_stdin_tx = Some(tx);
            console_input_task = Some(tokio::task::spawn_blocking(move || {
                let mut writer = master_writer;
                while let Some(data) = rx.blocking_recv() {
                    match data {
                        Some(bytes) => {
                            writer.write_all(&bytes)?;
                            writer.flush()?;
                        }
                        None => break,
                    }
                }
                Ok::<(), anyhow::Error>(())
            }));
        }
    } else {
        stdout_task = child
            .stdout
            .take()
            .zip(stdout_stream)
            .map(|(mut stdout, stream_id)| {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    loop {
                        match stdout.read(&mut buffer).await {
                            Ok(0) => {
                                writer.lock().await.write_data(stream_id, &[], true).await?;
                                break;
                            }
                            Ok(n) => {
                                writer
                                    .lock()
                                    .await
                                    .write_data(stream_id, &buffer[..n], false)
                                    .await?;
                            }
                            Err(e) => return Err(anyhow::Error::new(e)),
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                })
            });
        stderr_task = child
            .stderr
            .take()
            .zip(stderr_stream)
            .map(|(mut stderr, stream_id)| {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    loop {
                        match stderr.read(&mut buffer).await {
                            Ok(0) => {
                                writer.lock().await.write_data(stream_id, &[], true).await?;
                                break;
                            }
                            Ok(n) => {
                                writer
                                    .lock()
                                    .await
                                    .write_data(stream_id, &buffer[..n], false)
                                    .await?;
                            }
                            Err(e) => return Err(anyhow::Error::new(e)),
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                })
            });
    }

    let writer_for_input = writer.clone();
    let mut child_stdin = child.stdin.take();
    let console_stdin_tx_for_input = console_stdin_tx.clone();
    let stdin_task = tokio::spawn(async move {
        loop {
            match spdy::read_frame_async(&mut reader).await {
                Ok(spdy::Frame::Data(frame)) => {
                    if Some(frame.stream_id) == stdin_stream {
                        if req.tty {
                            if let Some(tx) = console_stdin_tx_for_input.as_ref() {
                                if !frame.data.is_empty() {
                                    tx.send(Some(frame.data)).await.map_err(|e| {
                                        anyhow::anyhow!("failed to forward tty stdin: {}", e)
                                    })?;
                                }
                                if frame.flags & 0x01 != 0 {
                                    let _ = tx.send(None).await;
                                }
                            }
                        } else if let Some(stdin) = child_stdin.as_mut() {
                            if !frame.data.is_empty() {
                                stdin.write_all(&frame.data).await?;
                            }
                            if frame.flags & 0x01 != 0 {
                                let _ = stdin.shutdown().await;
                                child_stdin = None;
                            }
                        }
                    }
                }
                Ok(spdy::Frame::Ping(frame)) => {
                    writer_for_input.lock().await.write_ping(frame.id).await?;
                }
                Ok(spdy::Frame::GoAway(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    log::debug!("Exec input loop stopped: {}", e);
                    break;
                }
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    let status = child.wait().await?;

    if let Some(tx) = console_stdin_tx.as_ref() {
        let _ = tx.send(None).await;
    }

    stdin_task.abort();
    let _ = stdin_task.await;
    if let Some(task) = console_input_task {
        task.await??;
    }

    let mut stdout_fin_sent = false;
    if let Some(task) = stdout_task {
        if req.tty {
            let mut task = task;
            match tokio::time::timeout(Duration::from_millis(200), &mut task).await {
                Ok(joined) => {
                    joined??;
                    stdout_fin_sent = true;
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        } else {
            task.await??;
            stdout_fin_sent = true;
        }
    }
    if let Some(task) = stderr_task {
        task.await??;
    }

    if let Some(stream_id) = error_stream {
        let mut writer = writer.lock().await;
        if req.tty && !stdout_fin_sent {
            if let Some(stdout_stream_id) = stdout_stream {
                writer.write_data(stdout_stream_id, &[], true).await?;
            }
        }
        if !status.success() {
            let exit_code = status.code().unwrap_or_default();
            writer
                .write_data(
                    stream_id,
                    format!("command terminated with non-zero exit code: {}", exit_code).as_bytes(),
                    false,
                )
                .await?;
        }
        writer.write_data(stream_id, &[], true).await?;
        writer.write_goaway(stream_id).await?;
    } else if let Some(stream_id) = stdout_stream.or(stderr_stream).or(stdin_stream) {
        writer.lock().await.write_goaway(stream_id).await?;
    }

    Ok(())
}

async fn serve_attach_spdy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    req: AttachRequest,
    _protocol: &'static str,
) -> anyhow::Result<()> {
    let upgraded = on_upgrade.await?;
    let (read_half, write_half) = tokio::io::split(upgraded);
    let writer = Arc::new(Mutex::new(spdy::AsyncSpdyWriter::new(write_half)));
    let mut reader = read_half;

    let attach_socket_path = format!("/var/run/crius/shims/{}/attach.sock", req.container_id);
    let shim = UnixStream::connect(&attach_socket_path).await?;
    let (mut shim_read, shim_write) = shim.into_split();
    let shim_write = Arc::new(Mutex::new(shim_write));

    let expected_roles = expected_attach_roles(&req);
    let mut header_decompressor = spdy::HeaderDecompressor::new();
    let mut stdin_stream = None;
    let mut stdout_stream = None;
    let mut stderr_stream = None;
    let mut error_stream = None;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while [error_stream, stdin_stream, stdout_stream, stderr_stream]
        .into_iter()
        .flatten()
        .count()
        < expected_roles.len()
    {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, spdy::read_frame_async(&mut reader)).await??;
        match frame {
            spdy::Frame::SynStream(frame) => {
                let headers =
                    spdy::decode_header_block(&frame.header_block, &mut header_decompressor)?;
                let stream_type = spdy::header_value(&headers, "streamtype")
                    .ok_or_else(|| anyhow::anyhow!("attach stream is missing streamtype header"))?;
                let role = match stream_type {
                    "error" => AttachStreamRole::Error,
                    "stdin" => AttachStreamRole::Stdin,
                    "stdout" => AttachStreamRole::Stdout,
                    "stderr" => AttachStreamRole::Stderr,
                    other => {
                        return Err(anyhow::anyhow!("unsupported attach streamtype {}", other))
                    }
                };

                if !expected_roles.contains(&role) {
                    return Err(anyhow::anyhow!(
                        "unexpected attach stream {:?} for request",
                        role
                    ));
                }

                match role {
                    AttachStreamRole::Error if error_stream.is_none() => {
                        error_stream = Some(frame.stream_id)
                    }
                    AttachStreamRole::Stdin if stdin_stream.is_none() => {
                        stdin_stream = Some(frame.stream_id)
                    }
                    AttachStreamRole::Stdout if stdout_stream.is_none() => {
                        stdout_stream = Some(frame.stream_id)
                    }
                    AttachStreamRole::Stderr if stderr_stream.is_none() => {
                        stderr_stream = Some(frame.stream_id)
                    }
                    _ => {
                        return Err(anyhow::anyhow!(
                            "duplicate attach stream {:?} received",
                            role
                        ));
                    }
                }

                writer
                    .lock()
                    .await
                    .write_syn_reply(frame.stream_id, &[], false)
                    .await?;
            }
            spdy::Frame::Ping(frame) => {
                writer.lock().await.write_ping(frame.id).await?;
            }
            _ => {}
        }
    }

    let writer_for_output = writer.clone();
    let error_stream_for_output = error_stream;
    let stdout_stream_for_output = stdout_stream;
    let stderr_stream_for_output = stderr_stream;
    tokio::spawn(async move {
        let mut decoder = AttachOutputDecoder::default();
        let mut buffer = [0u8; 8192];
        loop {
            match shim_read.read(&mut buffer).await {
                Ok(0) => {
                    if let Some(stream_id) = stdout_stream_for_output {
                        let _ = writer_for_output
                            .lock()
                            .await
                            .write_data(stream_id, &[], true)
                            .await;
                    }
                    if let Some(stream_id) = stderr_stream_for_output {
                        let _ = writer_for_output
                            .lock()
                            .await
                            .write_data(stream_id, &[], true)
                            .await;
                    }
                    if let Some(stream_id) = error_stream_for_output {
                        let _ = writer_for_output
                            .lock()
                            .await
                            .write_data(stream_id, &[], true)
                            .await;
                    }
                    if let Err(e) = decoder.finish() {
                        log::debug!("Attach output decoder finished with trailing data: {}", e);
                    }
                    if let Some(stream_id) = stdout_stream_for_output
                        .or(stderr_stream_for_output)
                        .or(error_stream_for_output)
                    {
                        let _ = writer_for_output.lock().await.write_goaway(stream_id).await;
                    }
                    break;
                }
                Ok(n) => match decoder.push(&buffer[..n]) {
                    Ok(frames) => {
                        for frame in frames {
                            let target_stream = match frame.pipe {
                                ATTACH_PIPE_STDOUT => stdout_stream_for_output,
                                ATTACH_PIPE_STDERR => {
                                    stderr_stream_for_output.or(stdout_stream_for_output)
                                }
                                _ => None,
                            };
                            if let Some(stream_id) = target_stream {
                                if writer_for_output
                                    .lock()
                                    .await
                                    .write_data(stream_id, &frame.payload, false)
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::debug!("Attach output decoder stopped: {}", e);
                        if let Some(stream_id) = error_stream_for_output {
                            let _ = writer_for_output
                                .lock()
                                .await
                                .write_data(stream_id, e.to_string().as_bytes(), false)
                                .await;
                        }
                        break;
                    }
                },
                Err(e) => {
                    log::debug!("Attach output pump stopped: {}", e);
                    break;
                }
            }
        }
    });

    loop {
        match spdy::read_frame_async(&mut reader).await {
            Ok(spdy::Frame::Data(frame)) => {
                if Some(frame.stream_id) == stdin_stream {
                    let mut shim_write = shim_write.lock().await;
                    if !frame.data.is_empty() {
                        shim_write.write_all(&frame.data).await?;
                    }
                    if frame.flags & 0x01 != 0 {
                        let _ = shim_write.shutdown().await;
                    }
                }
            }
            Ok(spdy::Frame::Ping(frame)) => {
                writer.lock().await.write_ping(frame.id).await?;
            }
            Ok(spdy::Frame::GoAway(_)) => break,
            Ok(_) => {}
            Err(e) => {
                log::debug!("Attach input loop stopped: {}", e);
                break;
            }
        }
    }

    Ok(())
}

fn response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(body.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_exec_url_generation() {
        let server = StreamingServer::for_test("http://127.0.0.1:12345");
        let req = ExecRequest {
            container_id: "abc".to_string(),
            cmd: vec!["sh".to_string()],
            stdin: true,
            stdout: true,
            stderr: false,
            tty: true,
        };

        let response = server.get_exec(&req).await.unwrap();
        assert!(response.url.contains("/exec/"));
        assert!(response.url.starts_with("http://127.0.0.1:12345"));
    }

    #[test]
    fn test_validate_exec_request_rejects_empty_command() {
        let req = ExecRequest {
            container_id: "abc".to_string(),
            cmd: Vec::new(),
            stdin: false,
            stdout: true,
            stderr: true,
            tty: false,
        };

        let err = StreamingServer::validate_exec_request(&req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn test_expected_exec_roles_omit_stderr_for_tty() {
        let req = ExecRequest {
            container_id: "abc".to_string(),
            cmd: vec!["sh".to_string()],
            stdin: true,
            stdout: true,
            stderr: false,
            tty: true,
        };

        assert_eq!(
            expected_exec_roles(&req),
            vec![
                ExecStreamRole::Error,
                ExecStreamRole::Stdin,
                ExecStreamRole::Stdout
            ]
        );
    }

    #[tokio::test]
    async fn test_attach_url_generation() {
        let server = StreamingServer::for_test("http://127.0.0.1:12345");
        let req = AttachRequest {
            container_id: "abc".to_string(),
            stdin: false,
            stdout: true,
            stderr: true,
            tty: false,
        };

        let response = server.get_attach(&req).await.unwrap();
        assert!(response.url.contains("/attach/"));
    }
}
