use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use anyhow::{Context, Result};
use log::{debug, error, info};
use nix::sys::stat::{makedev, mknod, Mode, SFlag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::oci::spec::{
    Linux, LinuxCapabilities, LinuxDeviceCgroup, LinuxResources, Mount, Process, Root, Spec, User,
};

pub mod shim_manager;
pub use shim_manager::{ShimManager, ShimConfig, ShimProcess};

/// 容器运行时接口
pub trait ContainerRuntime {
    /// 创建容器
    fn create_container(&self, config: &ContainerConfig) -> Result<String>;
    
    /// 启动容器
    fn start_container(&self, container_id: &str) -> Result<()>;
    
    /// 停止容器
    fn stop_container(&self, container_id: &str, timeout: Option<u32>) -> Result<()>;
    
    /// 删除容器
    fn remove_container(&self, container_id: &str) -> Result<()>;
    
    /// 获取容器状态
    fn container_status(&self, container_id: &str) -> Result<ContainerStatus>;
    
    /// 在容器中执行命令
    fn exec_in_container(&self, container_id: &str, command: &[String], tty: bool) -> Result<i32>;
}

/// 容器配置
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub mounts: Vec<MountConfig>,
    pub labels: Vec<(String, String)>,
    pub annotations: Vec<(String, String)>,
    pub privileged: bool,
    pub user: Option<String>,
    pub hostname: Option<String>,
    pub rootfs: PathBuf,
}

/// 挂载点配置
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub read_only: bool,
}

/// 容器状态
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerStatus {
    Created,
    Running,
    Stopped(i32), // 退出码
    Unknown,
}

/// runc容器状态
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuncState {
    #[serde(rename = "ociVersion")]
    oci_version: String,
    id: String,
    status: String,
    pid: i32,
    bundle: String,
    rootfs: String,
    created: String,
    owner: String,
}

/// 使用 runc 作为容器运行时
#[derive(Debug, Clone)]
pub struct RuncRuntime {
    runtime_path: PathBuf,
    root: PathBuf,
    shim_manager: Option<Arc<ShimManager>>,
}

impl RuncRuntime {
    fn unpack_layer_with_tar(layer_file: &Path, rootfs_dir: &Path) -> Result<()> {
        let output = Command::new("tar")
            .arg("-xzf")
            .arg(layer_file)
            .arg("-C")
            .arg(rootfs_dir)
            .arg("--no-same-owner")
            .arg("--no-same-permissions")
            .output()
            .with_context(|| format!("Failed to execute tar for {:?}", layer_file))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "Failed to unpack layer archive {:?}: {}",
                layer_file,
                stderr.trim()
            ));
        }
        Ok(())
    }

    fn normalize_image_id(id: &str) -> &str {
        id.strip_prefix("sha256:").unwrap_or(id)
    }

    fn image_id_matches(image_id: &str, candidate: &str) -> bool {
        if image_id == candidate {
            return true;
        }
        let normalized_image_id = Self::normalize_image_id(image_id);
        let normalized_candidate = Self::normalize_image_id(candidate);
        normalized_image_id == normalized_candidate
            || normalized_image_id.starts_with(normalized_candidate)
            || normalized_candidate.starts_with(normalized_image_id)
    }

    fn resolve_image_dir(&self, image_ref: &str) -> Result<PathBuf> {
        let images_dir = PathBuf::from("/var/lib/crius/storage/images");
        let entries = std::fs::read_dir(&images_dir)
            .with_context(|| format!("Failed to read images directory: {:?}", images_dir))?;

        for entry in entries {
            let entry = entry?;
            let image_dir = entry.path();
            if !image_dir.is_dir() {
                continue;
            }

            let metadata_path = image_dir.join("metadata.json");
            if !metadata_path.exists() {
                continue;
            }

            let metadata_bytes = std::fs::read(&metadata_path)
                .with_context(|| format!("Failed to read image metadata: {:?}", metadata_path))?;
            let metadata: serde_json::Value = serde_json::from_slice(&metadata_bytes)
                .with_context(|| format!("Failed to parse image metadata: {:?}", metadata_path))?;

            let image_id = metadata
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let repo_tags: Vec<String> = metadata
                .get("repo_tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            if repo_tags.iter().any(|tag| tag == image_ref)
                || Self::image_id_matches(image_id, image_ref)
            {
                return Ok(image_dir);
            }
        }

        Err(anyhow::anyhow!(
            "Image not found locally for reference: {}",
            image_ref
        ))
    }

    fn prepare_rootfs_from_image(&self, config: &ContainerConfig, container_id: &str) -> Result<()> {
        let rootfs_dir = config.rootfs.clone();
        if rootfs_dir.exists() {
            std::fs::remove_dir_all(&rootfs_dir)
                .with_context(|| format!("Failed to clean rootfs directory: {:?}", rootfs_dir))?;
        }
        std::fs::create_dir_all(&rootfs_dir)
            .with_context(|| format!("Failed to create rootfs directory: {:?}", rootfs_dir))?;

        let image_dir = self.resolve_image_dir(&config.image)?;
        let mut layer_files: Vec<PathBuf> = std::fs::read_dir(&image_dir)?
            .filter_map(|e| e.ok().map(|v| v.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("gz"))
            .collect();
        layer_files.sort_by_key(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });

        if layer_files.is_empty() {
            return Err(anyhow::anyhow!(
                "No image layers found in {:?} for image {}",
                image_dir,
                config.image
            ));
        }

        for layer_file in &layer_files {
            Self::unpack_layer_with_tar(layer_file, &rootfs_dir)
                .with_context(|| format!("Failed to unpack layer archive: {:?}", layer_file))?;
        }

        // Ensure minimum runtime paths exist for scratch-like images (e.g. pause).
        std::fs::create_dir_all(rootfs_dir.join("dev"))
            .context("Failed to create /dev in rootfs")?;
        std::fs::create_dir_all(rootfs_dir.join("proc"))
            .context("Failed to create /proc in rootfs")?;
        std::fs::create_dir_all(rootfs_dir.join("sys"))
            .context("Failed to create /sys in rootfs")?;

        let dev_null = rootfs_dir.join("dev/null");
        if dev_null.exists() {
            let _ = std::fs::remove_file(&dev_null);
        }
        mknod(
            &dev_null,
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o666),
            makedev(1, 3),
        )
        .context("Failed to create /dev/null char device in rootfs")?;

        info!(
            "Prepared rootfs for container {} from image {}",
            container_id, config.image
        );
        Ok(())
    }

    pub fn new(runtime_path: PathBuf, root: PathBuf) -> Self {
        Self { 
            runtime_path, 
            root,
            shim_manager: None,
        }
    }

    /// 创建带shim支持的运行时
    pub fn with_shim(runtime_path: PathBuf, root: PathBuf, shim_config: ShimConfig) -> Self {
        let shim_manager = Arc::new(ShimManager::new(shim_config));
        Self {
            runtime_path,
            root,
            shim_manager: Some(shim_manager),
        }
    }

    /// 启用shim支持
    pub fn enable_shim(&mut self, config: ShimConfig) {
        self.shim_manager = Some(Arc::new(ShimManager::new(config)));
    }

    /// 检查是否启用了shim
    pub fn is_shim_enabled(&self) -> bool {
        self.shim_manager.is_some()
    }

    /// 获取容器的bundle目录
    fn bundle_path(&self, container_id: &str) -> PathBuf {
        self.root.join(container_id)
    }

    /// 获取容器的rootfs目录
    fn rootfs_path(&self, container_id: &str) -> PathBuf {
        self.bundle_path(container_id).join("rootfs")
    }

    /// 获取容器的config.json路径
    fn config_path(&self, container_id: &str) -> PathBuf {
        self.bundle_path(container_id).join("config.json")
    }
    
    /// 执行runc命令并返回输出（仅用于需要解析stdout的查询类命令）
    fn run_command_output(&self, args: &[&str]) -> Result<Output> {
        debug!("Executing: {} {}", self.runtime_path.display(), args.join(" "));
        
        let output = Command::new(&self.runtime_path)
            .args(args)
            .env("XDG_RUNTIME_DIR", "/run/user/0")
            .output()
            .context("Failed to execute runc command")?;
            
        Ok(output)
    }

    /// 执行runc命令并检查状态（用于start/stop/run等动作类命令）
    /// 注意：不能对`runc run -d`使用output()，否则可能因后台子进程继承pipe导致阻塞。
    fn runc_exec(&self, args: &[&str]) -> Result<()> {
        debug!("Executing (status): {} {}", self.runtime_path.display(), args.join(" "));

        let status = Command::new(&self.runtime_path)
            .args(args)
            .env("XDG_RUNTIME_DIR", "/run/user/0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Failed to execute runc command")?;

        if !status.success() {
            let detail = self
                .run_command_output(args)
                .ok()
                .and_then(|out| {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    }
                })
                .unwrap_or_else(|| format!("status={}", status));
            error!("runc command failed: {}", detail);
            return Err(anyhow::anyhow!("runc command failed: {}", detail));
        }
        
        Ok(())
    }

    /// 创建OCI配置
    fn create_spec(&self, config: &ContainerConfig, _container_id: &str) -> Result<Spec> {
        let mut spec = Spec::new("1.0.2");

        // 设置root配置
        spec.root = Some(Root {
            path: config.rootfs.to_string_lossy().to_string(),
            readonly: Some(false),
        });

        // 设置进程配置
        let mut args = config.command.clone();
        if !config.args.is_empty() {
            args.extend(config.args.clone());
        }

        // 如果没有命令，使用默认shell
        if args.is_empty() {
            args = vec!["sh".to_string()];
        }

        // 转换环境变量为字符串格式
        let env: Vec<String> = config.env.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // 解析用户配置
        let user = config.user.as_ref().and_then(|u| {
            if let Ok(uid) = u.parse::<u32>() {
                Some(User {
                    uid,
                    gid: uid,
                    additional_gids: None,
                    username: None,
                })
            } else {
                None
            }
        });

        let default_caps = vec![
            "CAP_CHOWN".to_string(),
            "CAP_DAC_OVERRIDE".to_string(),
            "CAP_FSETID".to_string(),
            "CAP_FOWNER".to_string(),
            "CAP_MKNOD".to_string(),
            "CAP_NET_RAW".to_string(),
            "CAP_SETGID".to_string(),
            "CAP_SETUID".to_string(),
            "CAP_SETFCAP".to_string(),
            "CAP_SETPCAP".to_string(),
            "CAP_NET_BIND_SERVICE".to_string(),
            "CAP_SYS_CHROOT".to_string(),
            "CAP_KILL".to_string(),
            "CAP_AUDIT_WRITE".to_string(),
        ];

        spec.process = Some(Process {
            terminal: Some(false),
            user,
            args,
            env: if env.is_empty() { None } else { Some(env) },
            cwd: config.working_dir.as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".to_string()),
            capabilities: Some(LinuxCapabilities {
                bounding: Some(default_caps.clone()),
                effective: Some(default_caps.clone()),
                inheritable: Some(default_caps.clone()),
                permitted: Some(default_caps),
                ambient: Some(Vec::new()),
            }),
            rlimits: None,
            no_new_privileges: Some(!config.privileged),
            apparmor_profile: None,
            selinux_label: None,
        });

        // 设置主机名
        spec.hostname = config.hostname.clone();

        // 设置挂载点
        let default_mounts = Spec::default_mounts();
        let custom_mounts: Vec<Mount> = config.mounts.iter().map(|m| Mount {
            destination: m.destination.to_string_lossy().to_string(),
            source: Some(m.source.to_string_lossy().to_string()),
            mount_type: Some("bind".to_string()),
            options: if m.read_only {
                Some(vec!["rbind".to_string(), "ro".to_string()])
            } else {
                Some(vec!["rbind".to_string(), "rw".to_string()])
            },
        }).collect();

        let mut all_mounts: Vec<Mount> = default_mounts
            .into_iter()
            .filter(|m| m.destination != "/sys/fs/cgroup")
            .collect();
        all_mounts.extend(custom_mounts);
        spec.mounts = Some(all_mounts);

        // 设置Linux配置
        let namespaces = Spec::default_namespaces();
        let devices = if config.privileged {
            // 特权容器可以访问所有设备
            vec![]
        } else {
            Spec::default_devices()
        };

        spec.linux = Some(Linux {
            namespaces: Some(namespaces),
            uid_mappings: None,
            gid_mappings: None,
            devices: Some(devices),
            cgroups_path: None,
            resources: Some(LinuxResources {
                network: None,
                pids: None,
                memory: None,
                cpu: None,
                block_io: None,
                hugepage_limits: None,
                devices: Some(vec![LinuxDeviceCgroup {
                    allow: true,
                    device_type: None,
                    major: None,
                    minor: None,
                    access: Some("rwm".to_string()),
                }]),
                intel_rdt: None,
                unified: None,
            }),
            rootfs_propagation: None,
            seccomp: None,
            sysctl: None,
            mount_label: None,
            intel_rdt: None,
        });

        // 设置注解
        let mut annotations = std::collections::HashMap::new();
        annotations.insert("org.opencontainers.image.ref.name".to_string(), config.image.clone());
        for (k, v) in &config.annotations {
            annotations.insert(k.clone(), v.clone());
        }
        spec.annotations = Some(annotations);

        Ok(spec)
    }

    /// 创建bundle目录结构
    fn create_bundle(&self, container_id: &str, rootfs: &Path, spec: &Spec) -> Result<()> {
        let bundle_path = self.bundle_path(container_id);
        
        // 创建bundle目录
        std::fs::create_dir_all(&bundle_path)
            .context("Failed to create bundle directory")?;

        // 保存config.json
        spec.save(&self.config_path(container_id))?;

        // 当前运行时使用 OCI spec.root.path 作为 rootfs 来源，bundle 内不再强制准备 rootfs 目录。
        let _ = rootfs;

        info!("Created bundle for container {} at {:?}", container_id, bundle_path);
        Ok(())
    }

    /// 获取runc容器状态
    fn get_runc_state(&self, container_id: &str) -> Result<Option<RuncState>> {
        let output = self.run_command_output(&["state", container_id])?;
        
        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let state: RuncState = serde_json::from_str(&stdout)
            .context("Failed to parse runc state")?;
        
        Ok(Some(state))
    }
}

impl ContainerRuntime for RuncRuntime {
    fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        // 生成容器ID
        let container_id = Uuid::new_v4().to_simple().to_string();
        
        info!("Creating container {} with image {}", container_id, config.image);

        // 先从镜像构建rootfs，再生成OCI配置
        self.prepare_rootfs_from_image(config, &container_id)
            .context("Failed to prepare rootfs from image")?;

        // 创建OCI配置
        let spec = self.create_spec(config, &container_id)
            .context("Failed to create OCI spec")?;

        // 创建bundle
        self.create_bundle(&container_id, &config.rootfs, &spec)?;

        // 延迟到start阶段再调用runc，避免create阶段阻塞导致CRI超时。
        info!("Container {} bundle prepared successfully", container_id);
        Ok(container_id)
    }
    
    fn start_container(&self, container_id: &str) -> Result<()> {
        info!("Starting container {}", container_id);

        let state = self.get_runc_state(container_id)?;

        // 如果启用了shim，使用shim启动
        if let Some(ref shim_manager) = self.shim_manager {
            let bundle_path = self.bundle_path(container_id);
            let _process = shim_manager.start_shim(container_id, &bundle_path)?;
            info!("Container {} started via shim (PID: {})", container_id, _process.shim_pid);
        } else {
            match state {
                None => {
                    // runc run -d is used when this container has not been created in runc yet.
                    let bundle_path = self.bundle_path(container_id);
                    self.runc_exec(&[
                        "run",
                        "-d",
                        "--bundle",
                        &bundle_path.to_string_lossy(),
                        "--no-pivot",
                        container_id,
                    ])?;
                    info!("Container {} started via runc run -d", container_id);
                }
                Some(s) if s.status == "created" => {
                    self.runc_exec(&["start", container_id])?;
                    info!("Container {} started via runc start", container_id);
                }
                Some(s) if s.status == "running" => {
                    info!("Container {} already running", container_id);
                }
                Some(_) => {
                    self.runc_exec(&["start", container_id])?;
                    info!("Container {} started via runc start", container_id);
                }
            }
        }

        Ok(())
    }
    
    fn stop_container(&self, container_id: &str, timeout: Option<u32>) -> Result<()> {
        info!("Stopping container {}", container_id);

        // 如果启用了shim，先停止shim
        if let Some(ref shim_manager) = self.shim_manager {
            if shim_manager.is_shim_running(container_id) {
                shim_manager.stop_shim(container_id)?;
                info!("Shim for container {} stopped", container_id);
            }
        }

        // 获取当前状态
        let state = self.get_runc_state(container_id)?;
        
        match state {
            None => {
                info!("Container {} not found, already stopped", container_id);
                return Ok(());
            }
            Some(s) => {
                if s.status == "stopped" {
                    info!("Container {} already stopped", container_id);
                    return Ok(());
                }
            }
        }

        // 发送SIGTERM信号
        let signal = "TERM";
        self.runc_exec(&["kill", container_id, signal])?;

        // 等待容器停止
        let timeout_secs = timeout.unwrap_or(10);
        for _ in 0..timeout_secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            
            match self.get_runc_state(container_id)? {
                None => break,
                Some(s) if s.status == "stopped" => break,
                _ => continue,
            }
        }

        // 如果还在运行，发送SIGKILL
        if let Ok(Some(state)) = self.get_runc_state(container_id) {
            if state.status != "stopped" {
                info!("Container {} did not stop gracefully, sending SIGKILL", container_id);
                let _ = self.runc_exec(&["kill", container_id, "KILL"]);
            }
        }

        info!("Container {} stopped", container_id);
        Ok(())
    }
    
    fn remove_container(&self, container_id: &str) -> Result<()> {
        info!("Removing container {}", container_id);

        // 首先停止容器（如果还在运行）
        let _ = self.stop_container(container_id, Some(5));

        // 删除容器
        let output = self.run_command_output(&["delete", container_id])?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // 检查是否已经是"container does not exist"错误
            if stderr.contains("does not exist") {
                info!("Container {} does not exist", container_id);
            } else {
                return Err(anyhow::anyhow!("Failed to delete container: {}", stderr));
            }
        }

        // 清理bundle目录
        let bundle_path = self.bundle_path(container_id);
        if bundle_path.exists() {
            std::fs::remove_dir_all(&bundle_path)
                .context("Failed to remove bundle directory")?;
        }

        info!("Container {} removed", container_id);
        Ok(())
    }
    
    fn container_status(&self, container_id: &str) -> Result<ContainerStatus> {
        // 如果启用了shim，检查shim是否还在运行
        if let Some(ref shim_manager) = self.shim_manager {
            // 检查是否有退出码
            if let Ok(Some(exit_code)) = shim_manager.get_exit_code(container_id) {
                return Ok(ContainerStatus::Stopped(exit_code));
            }
            
            // 检查shim是否还在运行
            if shim_manager.is_shim_running(container_id) {
                return Ok(ContainerStatus::Running);
            }
        }

        // 回退到runc状态查询
        match self.get_runc_state(container_id)? {
            None => Ok(ContainerStatus::Unknown),
            Some(state) => {
                let status = match state.status.as_str() {
                    "created" => ContainerStatus::Created,
                    "running" => ContainerStatus::Running,
                    "stopped" => ContainerStatus::Stopped(0),
                    _ => ContainerStatus::Unknown,
                };
                Ok(status)
            }
        }
    }

    fn exec_in_container(&self, container_id: &str, command: &[String], tty: bool) -> Result<i32> {
        info!("Executing command in container {}: {:?}", container_id, command);

        let mut cmd = std::process::Command::new(&self.runtime_path);
        cmd.arg("exec");
        
        if tty {
            cmd.arg("-t");
        }
        cmd.arg("-i"); // 始终启用stdin交互
        
        // 添加容器ID
        cmd.arg(container_id);
        
        // 添加命令
        for arg in command {
            cmd.arg(arg);
        }

        // 执行命令并等待结果
        let output = cmd.output()
            .context("Failed to execute runc exec")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("exec failed: {}", stderr));
        }

        // 返回退出码
        let exit_code = output.status.code().unwrap_or(0);
        info!("Command executed in container {} with exit code {}", container_id, exit_code);
        Ok(exit_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_runtime() -> (RuncRuntime, tempfile::TempDir) {
        let temp_dir = tempdir().unwrap();
        let runtime = RuncRuntime::new(
            PathBuf::from("runc"),
            temp_dir.path().join("containers"),
        );
        (runtime, temp_dir)
    }

    fn create_test_config() -> ContainerConfig {
        ContainerConfig {
            name: "test".to_string(),
            image: "test:latest".to_string(),
            command: vec!["echo".to_string(), "hello".to_string()],
            args: vec![],
            env: vec![],
            working_dir: None,
            mounts: vec![],
            labels: vec![],
            annotations: vec![],
            privileged: false,
            user: None,
            hostname: None,
            rootfs: PathBuf::from("/tmp/rootfs"),
        }
    }

    #[test]
    fn test_create_spec() {
        let (runtime, _temp) = create_test_runtime();
        let config = create_test_config();
        
        let spec = runtime.create_spec(&config, "test-id").unwrap();
        
        assert_eq!(spec.oci_version, "1.0.2");
        assert!(spec.process.is_some());
        assert!(spec.root.is_some());
        assert!(spec.linux.is_some());
    }

    #[test]
    fn test_bundle_path() {
        let (runtime, _temp) = create_test_runtime();
        let path = runtime.bundle_path("test-container");
        assert!(path.to_string_lossy().contains("test-container"));
    }

    #[test]
    fn test_spec_with_custom_mounts() {
        let (runtime, _temp) = create_test_runtime();
        let mut config = create_test_config();
        config.mounts = vec![
            MountConfig {
                source: PathBuf::from("/host/path"),
                destination: PathBuf::from("/container/path"),
                read_only: true,
            }
        ];
        
        let spec = runtime.create_spec(&config, "test-id").unwrap();
        let mounts = spec.mounts.unwrap();
        
        // Should have default mounts + 1 custom mount
        assert!(mounts.len() > 1);
        assert!(mounts.iter().any(|m| m.destination == "/container/path"));
    }

    #[test]
    fn test_spec_with_user() {
        let (runtime, _temp) = create_test_runtime();
        let mut config = create_test_config();
        config.user = Some("1000".to_string());
        
        let spec = runtime.create_spec(&config, "test-id").unwrap();
        let process = spec.process.unwrap();
        let user = process.user.unwrap();
        
        assert_eq!(user.uid, 1000);
        assert_eq!(user.gid, 1000);
    }

    #[test]
    fn test_spec_privileged() {
        let (runtime, _temp) = create_test_runtime();
        let mut config = create_test_config();
        config.privileged = true;
        
        let spec = runtime.create_spec(&config, "test-id").unwrap();
        let linux = spec.linux.unwrap();
        
        // Privileged containers have empty device list
        assert!(linux.devices.unwrap().is_empty());
    }
}