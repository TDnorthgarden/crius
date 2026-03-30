//! IO管理 - 处理容器的stdin/stdout/stderr
//!
//! 功能：
//! 1. 重定向容器IO流到文件或socket
//! 2. 支持attach功能（多客户端连接）
//! 3. 支持TTY模式
//! 4. 日志持久化

use std::path::PathBuf;
use std::fs::File;
use std::io::{self, Write, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use anyhow::{Result, Context};
use log::{info, debug, error};

/// IO配置
#[derive(Debug, Clone)]
pub struct IoConfig {
    /// stdin重定向文件
    pub stdin: Option<PathBuf>,
    /// stdout重定向文件
    pub stdout: Option<PathBuf>,
    /// stderr重定向文件
    pub stderr: Option<PathBuf>,
    /// TTY模式
    pub terminal: bool,
    /// attach socket地址
    pub attach_socket: Option<PathBuf>,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            stdin: None,
            stdout: None,
            stderr: None,
            terminal: false,
            attach_socket: None,
        }
    }
}

/// IO管理器
pub struct IoManager {
    config: IoConfig,
    /// 当前attach的客户端
    clients: Arc<Mutex<Vec<ClientConnection>>>,
    /// 日志文件
    log_file: Option<File>,
}

/// 客户端连接
#[derive(Debug)]
struct ClientConnection {
    id: usize,
    stream: UnixStream,
}

impl IoManager {
    /// 创建新的IO管理器
    pub fn new() -> Self {
        Self {
            config: IoConfig::default(),
            clients: Arc::new(Mutex::new(Vec::new())),
            log_file: None,
        }
    }

    /// 配置IO
    pub fn configure(&mut self, config: IoConfig) -> Result<()> {
        self.config = config.clone();

        // 设置日志文件
        if let Some(stdout) = &config.stdout {
            let parent = stdout.parent().context("Invalid stdout path")?;
            std::fs::create_dir_all(parent)?;
            self.log_file = Some(File::create(stdout)?);
            info!("Log file configured: {:?}", stdout);
        }

        Ok(())
    }

    /// 启动attach服务器
    pub fn start_attach_server(&mut self) -> Result<()> {
        if let Some(socket) = &self.config.attach_socket {
            // 删除已存在的socket文件
            let _ = std::fs::remove_file(socket);
            
            let listener = UnixListener::bind(socket)?;
            info!("Attach server listening on {:?}", socket);

            let clients = self.clients.clone();

            std::thread::spawn(move || {
                for (id, stream) in listener.incoming().enumerate() {
                    match stream {
                        Ok(stream) => {
                            debug!("New attach client connected: {}", id);
                            let mut clients = clients.lock().unwrap();
                            clients.push(ClientConnection { id, stream });
                        }
                        Err(e) => {
                            error!("Failed to accept client: {}", e);
                        }
                    }
                }
            });
        }

        Ok(())
    }

    /// 写入stdout
    pub fn write_stdout(&mut self, data: &[u8]) -> Result<()> {
        // 写入日志文件
        if let Some(file) = &mut self.log_file {
            file.write_all(data)?;
            file.flush()?;
        }

        // 发送到所有attach客户端
        let mut clients = self.clients.lock().unwrap();
        clients.retain_mut(|client| {
            match client.stream.write_all(data) {
                Ok(_) => true,
                Err(e) => {
                    debug!("Client {} disconnected: {}", client.id, e);
                    false
                }
            }
        });

        Ok(())
    }

    /// 写入stderr
    pub fn write_stderr(&mut self, data: &[u8]) -> Result<()> {
        // 如果有单独的stderr文件，写入
        // 否则按stdout处理
        self.write_stdout(data)
    }

    /// 读取stdin
    pub fn read_stdin(&mut self) -> Result<Vec<u8>> {
        // 从第一个客户端读取（如果有）
        let mut clients = self.clients.lock().unwrap();
        if let Some(client) = clients.first_mut() {
            let mut buffer = [0u8; 1024];
            match client.stream.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    return Ok(buffer[..n].to_vec());
                }
                _ => {}
            }
        }
        Ok(Vec::new())
    }

    /// 设置容器IO重定向（用于runc）
    pub fn setup_stdio_pipes(&self) -> Result<(Option<File>, Option<File>, Option<File>)> {
        // 创建管道用于与容器通信
        // stdin
        let stdin = if self.config.stdin.is_some() {
            let file = File::open(self.config.stdin.as_ref().unwrap())?;
            Some(file)
        } else {
            None
        };

        // stdout - 直接写入日志文件
        let stdout = if let Some(stdout) = &self.config.stdout {
            let parent = stdout.parent().context("Invalid stdout path")?;
            std::fs::create_dir_all(parent)?;
            let file = File::create(stdout)?;
            Some(file)
        } else {
            None
        };

        // stderr
        let stderr = if let Some(stderr) = &self.config.stderr {
            let parent = stderr.parent().context("Invalid stderr path")?;
            std::fs::create_dir_all(parent)?;
            let file = File::create(stderr)?;
            Some(file)
        } else {
            None
        };

        Ok((stdin, stdout, stderr))
    }

    /// 关闭所有客户端连接
    pub fn shutdown(&mut self) -> Result<()> {
        info!("Shutting down IO manager");

        let mut clients = self.clients.lock().unwrap();
        clients.clear();

        if let Some(file) = &mut self.log_file {
            file.flush()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_io_manager_creation() {
        let manager = IoManager::new();
        assert!(!manager.config.terminal);
    }

    #[test]
    fn test_io_config_default() {
        let config = IoConfig::default();
        assert!(!config.terminal);
        assert!(config.stdin.is_none());
        assert!(config.stdout.is_none());
        assert!(config.stderr.is_none());
    }

    #[test]
    fn test_io_manager_configure() {
        let temp_dir = tempdir().unwrap();
        let mut manager = IoManager::new();

        let config = IoConfig {
            stdout: Some(temp_dir.path().join("stdout.log")),
            ..Default::default()
        };

        manager.configure(config).unwrap();
        assert!(manager.log_file.is_some());
    }
}
