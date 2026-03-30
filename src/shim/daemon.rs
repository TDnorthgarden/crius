//! Shim守护进程实现
//!
//! 守护进程负责：
//! 1. 设置子进程收割（PR_SET_CHILD_SUBREAPER）
//! 2. 创建容器进程（通过runc create）
//! 3. 监控容器进程生命周期
//! 4. 记录容器退出码
//! 5. 管理IO流

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::fs;
use anyhow::{Context, Result};
use log::{info, debug, error, warn};
use nix::unistd::{self, Pid};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

use crate::shim::io::{IoManager, IoConfig};

/// Shim守护进程
pub struct Daemon {
    /// 容器ID
    container_id: String,
    /// Bundle目录
    bundle: PathBuf,
    /// Runtime路径
    runtime: PathBuf,
    /// 退出码文件路径
    exit_code_file: Option<PathBuf>,
    /// IO管理器
    io_manager: IoManager,
    /// 容器进程PID
    container_pid: Arc<AtomicI32>,
    /// 是否正在运行
    running: Arc<AtomicBool>,
}

impl Daemon {
    /// 创建新的守护进程
    pub fn new(
        container_id: String,
        bundle: PathBuf,
        runtime: PathBuf,
        exit_code_file: Option<PathBuf>,
    ) -> Self {
        Self {
            container_id,
            bundle,
            runtime,
            exit_code_file,
            io_manager: IoManager::new(),
            container_pid: Arc::new(AtomicI32::new(-1)),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// 运行守护进程
    pub fn run(self) -> Result<()> {
        // 1. 设置子进程收割者
        self.setup_subreaper()?;

        // 2. 设置信号处理器
        self.setup_signal_handlers()?;

        // 3. 初始化IO
        self.setup_io()?;

        // 4. 创建容器（runc create）
        let container_pid = self.create_container()?;
        info!("Container created with PID: {}", container_pid);

        // 5. 监控容器进程
        let exit_code = self.monitor_container(container_pid)?;

        // 6. 记录退出码
        self.record_exit_code(exit_code)?;

        info!("Shim daemon exiting with container exit code: {}", exit_code);
        std::process::exit(exit_code);
    }

    /// 设置子进程收割者
    fn setup_subreaper(&self) -> Result<()> {
        // 使用libc直接调用prctl设置子进程收割者
        // PR_SET_CHILD_SUBREAPER = 36
        const PR_SET_CHILD_SUBREAPER: i32 = 36;
        let result = unsafe { libc::prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        
        if result != 0 {
            return Err(anyhow::anyhow!("Failed to set subreaper: {}", std::io::Error::last_os_error()));
        }

        info!("Set as child subreaper");
        Ok(())
    }

    /// 设置信号处理器
    fn setup_signal_handlers(&self) -> Result<()> {
        // 处理SIGCHLD信号
        let running = self.running.clone();

        ctrlc::set_handler(move || {
            info!("Received SIGINT/SIGTERM, shutting down...");
            running.store(false, Ordering::SeqCst);
        }).context("Failed to set signal handler")?;

        Ok(())
    }

    /// 设置IO
    fn setup_io(&self) -> Result<()> {
        // 初始化IO管理器
        // 可以设置stdin/stdout/stderr重定向
        info!("IO setup complete");
        Ok(())
    }

    /// 创建容器
    fn create_container(&self) -> Result<Pid> {
        // 首先检查容器是否已经存在
        let state_output = Command::new(&self.runtime)
            .args(&["state", &self.container_id])
            .output()?;

        if state_output.status.success() {
            // 容器已存在，获取其PID
            let state: serde_json::Value = serde_json::from_slice(&state_output.stdout)?;
            if let Some(pid) = state.get("pid").and_then(|p| p.as_i64()) {
                return Ok(Pid::from_raw(pid as i32));
            }
        }

        // 创建新容器
        info!("Creating container: {}", self.container_id);

        let output = Command::new(&self.runtime)
            .args(&[
                "create",
                "--bundle", self.bundle.to_str().unwrap(),
                &self.container_id,
            ])
            .output()
            .context("Failed to execute runc create")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("runc create failed: {}", stderr);
            return Err(anyhow::anyhow!("Failed to create container: {}", stderr));
        }

        // 获取容器PID
        let state_output = Command::new(&self.runtime)
            .args(&["state", &self.container_id])
            .output()?;

        if !state_output.status.success() {
            return Err(anyhow::anyhow!("Failed to get container state after creation"));
        }

        let state: serde_json::Value = serde_json::from_slice(&state_output.stdout)?;
        let pid = state.get("pid")
            .and_then(|p| p.as_i64())
            .context("Failed to parse container PID from state")?;

        // 启动容器
        let output = Command::new(&self.runtime)
            .args(&["start", &self.container_id])
            .output()
            .context("Failed to execute runc start")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("runc start warning: {}", stderr);
            // 继续执行，因为某些情况下容器可能已经启动
        }

        Ok(Pid::from_raw(pid as i32))
    }

    /// 监控容器进程
    fn monitor_container(&self, container_pid: Pid) -> Result<i32> {
        info!("Monitoring container process: {}", container_pid);

        let mut exit_code = 0;

        while self.running.load(Ordering::SeqCst) {
            // 等待子进程状态变化
            match waitpid(Some(container_pid), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_pid, code)) => {
                    info!("Container exited with code: {}", code);
                    exit_code = code;
                    break;
                }
                Ok(WaitStatus::Signaled(_pid, signal, _)) => {
                    info!("Container killed by signal: {:?}", signal);
                    exit_code = 128 + signal as i32; // 标准shell约定
                    break;
                }
                Ok(_) => {
                    // 仍在运行或其他状态
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    error!("Error waiting for container: {}", e);
                    break;
                }
            }
        }

        // 清理容器状态
        self.cleanup_container()?;

        Ok(exit_code)
    }

    /// 清理容器
    fn cleanup_container(&self) -> Result<()> {
        info!("Cleaning up container: {}", self.container_id);

        // 尝试删除容器
        let output = Command::new(&self.runtime)
            .args(&["delete", &self.container_id])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Container cleanup warning: {}", stderr);
        }

        Ok(())
    }

    /// 记录退出码
    fn record_exit_code(&self, exit_code: i32) -> Result<()> {
        if let Some(path) = &self.exit_code_file {
            let parent = path.parent().context("Invalid exit code file path")?;
            fs::create_dir_all(parent)?;

            fs::write(path, exit_code.to_string())
                .context("Failed to write exit code file")?;

            info!("Recorded exit code {} to {:?}", exit_code, path);
        }

        Ok(())
    }
}
