use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::storage::persistence::{PersistenceManager, PersistenceConfig};
use crate::proto::runtime::v1::{
    runtime_service_server::RuntimeService, Container, ContainerState, ContainerStatus as CriContainerStatus,
    ExecResponse, ExecSyncResponse,
    PortForwardResponse, RunPodSandboxRequest, RunPodSandboxResponse,
    StatusResponse, VersionRequest, VersionResponse,StopPodSandboxRequest,StopPodSandboxResponse,
    ExecRequest,
    ExecSyncRequest,PortForwardRequest,
    StatusRequest,
};
use crate::proto::runtime::v1::{
    RemovePodSandboxRequest,RemovePodSandboxResponse,
    GetEventsRequest,ContainerEventResponse,
    ListMetricDescriptorsRequest,ListMetricDescriptorsResponse,
    ListPodSandboxMetricsRequest,ListPodSandboxMetricsResponse,
    RuntimeConfigRequest,RuntimeConfigResponse,
    CheckpointContainerRequest,CheckpointContainerResponse,
    PodSandboxStatusRequest,PodSandboxStatusResponse,
    ListPodSandboxRequest,ListPodSandboxResponse,
    CreateContainerRequest,CreateContainerResponse,
    StartContainerRequest,StartContainerResponse,
    StopContainerRequest,StopContainerResponse,
    RemoveContainerRequest,RemoveContainerResponse,
    ListContainersRequest,ListContainersResponse,
    ContainerStatusRequest,ContainerStatusResponse,
    ReopenContainerLogRequest,ReopenContainerLogResponse,
    AttachRequest,AttachResponse,
    ContainerStatsRequest,ContainerStatsResponse,
    ListContainerStatsRequest,ListContainerStatsResponse,
    PodSandboxStatsRequest,PodSandboxStatsResponse,
    ListPodSandboxStatsRequest,ListPodSandboxStatsResponse,
    UpdateRuntimeConfigRequest,UpdateRuntimeConfigResponse,
    UpdateContainerResourcesRequest,UpdateContainerResourcesResponse,
    PodSandboxState,PodSandboxStatus,
};

use crate::runtime::{ContainerRuntime, RuncRuntime, ContainerConfig, MountConfig, ContainerStatus};
use crate::pod::{PodSandboxManager, PodSandboxConfig};
use crate::storage::{ContainerRecord, PodSandboxRecord};

/// 运行时服务实现
#[derive(Debug)]
pub struct RuntimeServiceImpl {
    // 存储容器状态的线程安全HashMap
    containers: Arc<Mutex<HashMap<String, Container>>>,
    // 存储Pod沙箱状态的线程安全HashMap
    pod_sandboxes: Arc<Mutex<HashMap<String, crate::proto::runtime::v1::PodSandbox>>>,
    // 运行时配置
    config: RuntimeConfig,
    // 容器运行时
    runtime: RuncRuntime,
    // Pod沙箱管理器
    pod_manager: tokio::sync::Mutex<PodSandboxManager<RuncRuntime>>,
    // 持久化管理器
    persistence: Arc<Mutex<PersistenceManager>>,
}

/// 运行时配置
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub root_dir: PathBuf,
    pub runtime: String,
    pub runtime_root: PathBuf,
    pub log_dir: PathBuf,
    pub runtime_path: PathBuf,
}

impl RuntimeServiceImpl {
    pub fn new(config: RuntimeConfig) -> Self {
        let runtime = RuncRuntime::new(
            config.runtime_path.clone(),
            config.runtime_root.clone(),
        );
        
        let pod_manager = PodSandboxManager::new(
            runtime.clone(),
            config.root_dir.join("pods"),
        );
        
        // 初始化持久化管理器
        let persistence_config = PersistenceConfig {
            db_path: config.root_dir.join("crius.db"),
            enable_recovery: true,
            auto_save_interval: 30,
        };
        
        let persistence = PersistenceManager::new(persistence_config)
            .expect("Failed to create persistence manager");
        
        Self {
            containers: Arc::new(Mutex::new(HashMap::new())),
            pod_sandboxes: Arc::new(Mutex::new(HashMap::new())),
            config,
            runtime,
            pod_manager: tokio::sync::Mutex::new(pod_manager),
            persistence: Arc::new(Mutex::new(persistence)),
        }
    }
    
    /// 从持久化存储恢复状态
    pub async fn recover_state(&self) -> Result<(), Status> {
        let mut persistence = self.persistence.lock().await;
        
        // 恢复容器状态
        match persistence.recover_containers() {
            Ok(containers) => {
                let mut memory_containers = self.containers.lock().await;
                for (id, _status, record) in containers {
                    // 从记录重建容器元数据
                    let container = Container {
                        id: id.clone(),
                        pod_sandbox_id: record.pod_id,
                        state: match record.state.as_str() {
                            "created" => ContainerState::ContainerCreated as i32,
                            "running" => ContainerState::ContainerRunning as i32,
                            "stopped" => ContainerState::ContainerExited as i32,
                            _ => ContainerState::ContainerCreated as i32,
                        },
                        created_at: record.created_at,
                        image_ref: record.image,
                        labels: serde_json::from_str(&record.labels).unwrap_or_default(),
                        annotations: serde_json::from_str(&record.annotations).unwrap_or_default(),
                        ..Default::default()
                    };
                    memory_containers.insert(id, container);
                }
                log::info!("Recovered {} containers from database", memory_containers.len());
            }
            Err(e) => {
                log::error!("Failed to recover containers: {}", e);
            }
        }
        
        // 恢复Pod沙箱状态
        match persistence.recover_pods() {
            Ok(pods) => {
                let mut memory_pods = self.pod_sandboxes.lock().await;
                for record in pods {
                    let pod = crate::proto::runtime::v1::PodSandbox {
                        id: record.id.clone(),
                        state: match record.state.as_str() {
                            "ready" => PodSandboxState::SandboxReady as i32,
                            "notready" => PodSandboxState::SandboxNotready as i32,
                            _ => PodSandboxState::SandboxNotready as i32,
                        },
                        created_at: record.created_at,
                        labels: serde_json::from_str(&record.labels).unwrap_or_default(),
                        annotations: serde_json::from_str(&record.annotations).unwrap_or_default(),
                        ..Default::default()
                    };
                    memory_pods.insert(record.id, pod);
                }
                log::info!("Recovered {} pod sandboxes from database", memory_pods.len());
            }
            Err(e) => {
                log::error!("Failed to recover pod sandboxes: {}", e);
            }
        }
        
        Ok(())
    }
}

#[tonic::async_trait]
impl RuntimeService for RuntimeServiceImpl {
    // 获取运行时版本
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".to_string(),
            runtime_name: "crius".to_string(),
            runtime_version: "0.1.0".to_string(),
            runtime_api_version: "v1".to_string(),
        }))
    }

    // 创建Pod沙箱
    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let pod_config = req.config.ok_or_else(|| Status::invalid_argument("Pod config not specified"))?;
        
        // 构建Pod沙箱配置
        let sandbox_config = PodSandboxConfig {
            name: pod_config.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            namespace: pod_config.metadata.as_ref().map(|m| m.namespace.clone()).unwrap_or_else(|| "default".to_string()),
            uid: pod_config.metadata.as_ref().map(|m| m.uid.clone()).unwrap_or_default(),
            hostname: pod_config.hostname.clone(),
            labels: pod_config.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            annotations: pod_config.annotations.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            dns_config: pod_config.dns_config.map(|d| crate::pod::DNSConfig {
                servers: d.servers,
                searches: d.searches,
                options: d.options,
            }),
            port_mappings: pod_config.port_mappings.iter().map(|p| {
                // protocol是i32枚举，需要转换为字符串
                let protocol_str = match p.protocol {
                    0 => "TCP",
                    1 => "UDP", 
                    2 => "SCTP",
                    _ => "TCP",
                }.to_string();
                crate::pod::PortMapping {
                    protocol: protocol_str,
                    container_port: p.container_port,
                    host_port: p.host_port,
                    host_ip: p.host_ip.clone(),
                }
            }).collect(),
            network_config: None,
        };

        // 创建Pod沙箱
        let mut pod_manager = self.pod_manager.lock().await;
        let pod_id = pod_manager.create_pod_sandbox(sandbox_config).await
            .map_err(|e| Status::internal(format!("Failed to create pod sandbox: {}", e)))?;
        
        // 创建Pod沙箱元数据
        let pod_sandbox = crate::proto::runtime::v1::PodSandbox {
            id: pod_id.clone(),
            metadata: pod_config.metadata.clone(),
            state: PodSandboxState::SandboxReady as i32,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            labels: pod_config.labels.clone(),
            annotations: pod_config.annotations.clone(),
            ..Default::default()
        };
        
        // 存储Pod沙箱信息到内存
        let mut pod_sandboxes = self.pod_sandboxes.lock().await;
        pod_sandboxes.insert(pod_id.clone(), pod_sandbox.clone());
        drop(pod_sandboxes);
        
        // 持久化Pod沙箱状态
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.save_pod_sandbox(
            &pod_id,
            "ready",
            &pod_config.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            &pod_config.metadata.as_ref().map(|m| m.namespace.clone()).unwrap_or_else(|| "default".to_string()),
            &pod_config.metadata.as_ref().map(|m| m.uid.clone()).unwrap_or_default(),
            &format!("/var/run/netns/crius-{}-{}", 
                pod_config.metadata.as_ref().map(|m| m.namespace.clone()).unwrap_or_else(|| "default".to_string()),
                pod_config.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default()),
            &pod_config.labels,
            &pod_config.annotations,
            None, // pause_container_id will be set later
            None, // ip will be set later
        ) {
            log::error!("Failed to persist pod sandbox {}: {}", pod_id, e);
        } else {
            log::info!("Pod sandbox {} persisted to database", pod_id);
        }
        
        log::info!("Pod sandbox {} created successfully", pod_id);
        Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: pod_id }))
    }

    // 停止Pod沙箱
    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let pod_id = req.pod_sandbox_id;
        
        log::info!("Stopping pod sandbox {}", pod_id);
        
        // 停止Pod沙箱
        let mut pod_manager = self.pod_manager.lock().await;
        pod_manager.stop_pod_sandbox(&pod_id).await
            .map_err(|e| Status::internal(format!("Failed to stop pod sandbox: {}", e)))?;
        
        // 更新Pod沙箱状态到内存
        let mut pod_sandboxes = self.pod_sandboxes.lock().await;
        if let Some(pod) = pod_sandboxes.get_mut(&pod_id) {
            pod.state = PodSandboxState::SandboxNotready as i32;
        }
        drop(pod_sandboxes);
        
        // 更新持久化状态
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.update_pod_state(&pod_id, "notready") {
            log::error!("Failed to update pod {} state in database: {}", pod_id, e);
        }
        
        log::info!("Pod sandbox {} stopped", pod_id);
        Ok(Response::new(StopPodSandboxResponse { }))
    }

    // 获取容器状态
    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        let containers = self.containers.lock().await;
        
        if let Some(container) = containers.get(&container_id) {
            // 查询runtime获取实时状态
            let runtime_state = match self.runtime.container_status(&container_id) {
                Ok(status) => match status {
                    ContainerStatus::Created => ContainerState::ContainerCreated,
                    ContainerStatus::Running => ContainerState::ContainerRunning,
                    ContainerStatus::Stopped(_) => ContainerState::ContainerExited,
                    ContainerStatus::Unknown => ContainerState::ContainerUnknown,
                },
                Err(_) => ContainerState::ContainerUnknown,
            };

            let status = CriContainerStatus {
                id: container.id.clone(),
                state: runtime_state as i32,
                created_at: container.created_at,
                image_ref: container.image_ref.clone(),
                ..Default::default()
            };
            
            Ok(Response::new(ContainerStatusResponse {
                status: Some(status),
                ..Default::default()
            }))
        } else {
            Err(Status::not_found("Container not found"))
        }
    }

    // 列出容器
    async fn list_containers(
        &self,
        _request: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let containers = self.containers.lock().await;
        let containers_list = containers.values().cloned().collect();
        
        Ok(Response::new(ListContainersResponse {
            containers: containers_list,
        }))
    }

    // 执行命令
    async fn exec(
        &self,
        _request: Request<ExecRequest>,
    ) -> Result<Response<ExecResponse>, Status> {
        // 实现执行命令的逻辑
        Ok(Response::new(ExecResponse {
            url: "unix:///var/run/crius/crius.sock".to_string(),
        }))
    }

    // 同步执行命令
    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        let cmd = req.cmd;
        let timeout = req.timeout;
        
        log::info!("Exec sync in container {}: {:?}", container_id, cmd);

        // 检查容器是否存在
        let containers = self.containers.lock().await;
        if !containers.contains_key(&container_id) {
            return Err(Status::not_found("Container not found"));
        }
        drop(containers);

        // 调用runtime执行命令
        let runtime = self.runtime.clone();
        let container_id_clone = container_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            runtime.exec_in_container(&container_id_clone, &cmd, false)
        }).await;

        match result {
            Ok(Ok(exit_code)) => {
                Ok(Response::new(ExecSyncResponse {
                    stdout: Vec::new(), // TODO: 捕获stdout
                    stderr: Vec::new(), // TODO: 捕获stderr
                    exit_code,
                }))
            }
            Ok(Err(e)) => {
                log::error!("Exec failed in container {}: {}", container_id, e);
                Err(Status::internal(format!("Exec failed: {}", e)))
            }
            Err(e) => {
                Err(Status::internal(format!("Failed to spawn blocking task: {}", e)))
            }
        }
    }

    // 端口转发
    async fn port_forward(
        &self,
        _request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        // 实现端口转发的逻辑
        Ok(Response::new(PortForwardResponse {
            url: "unix:///var/run/crius/crius.sock".to_string(),
        }))
    }

    // 获取运行时状态
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let mut info = HashMap::new();
        info.insert("runtime_name".to_string(), "crius".to_string());
        info.insert("runtime_version".to_string(), "0.1.0".to_string());
        info.insert("runtime_api_version".to_string(), "v1".to_string());
        info.insert("root_dir".to_string(), self.config.root_dir.to_string_lossy().to_string());
        info.insert("runtime".to_string(), self.config.runtime.clone());
        
        Ok(Response::new(StatusResponse {
            status: Some(crate::proto::runtime::v1::RuntimeStatus {
                conditions: Vec::new(),
            }),
            info,
        }))
    }

    // 删除pod_sandbox
    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let req = request.into_inner();
        let pod_id = req.pod_sandbox_id;
        
        log::info!("Removing pod sandbox {}", pod_id);
        
        // 删除Pod沙箱
        let mut pod_manager = self.pod_manager.lock().await;
        pod_manager.remove_pod_sandbox(&pod_id).await
            .map_err(|e| Status::internal(format!("Failed to remove pod sandbox: {}", e)))?;
        
        // 从内存中移除
        let mut pod_sandboxes = self.pod_sandboxes.lock().await;
        pod_sandboxes.remove(&pod_id);
        drop(pod_sandboxes);
        
        // 从持久化存储中删除
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.delete_pod_sandbox(&pod_id) {
            log::error!("Failed to delete pod {} from database: {}", pod_id, e);
        } else {
            log::info!("Pod sandbox {} removed from database", pod_id);
        }
        
        log::info!("Pod sandbox {} removed", pod_id);
        Ok(Response::new(RemovePodSandboxResponse { }))
    }

    // 获取pod_sandbox状态
    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let req = request.into_inner();
        let pod_sandboxes = self.pod_sandboxes.lock().await;
        
        if let Some(pod_sandbox) = pod_sandboxes.get(&req.pod_sandbox_id) {
            let mut info = HashMap::new();
            info.insert("podSandboxId".to_string(), pod_sandbox.id.clone());
            if let Some(metadata) = &pod_sandbox.metadata {
                info.insert("name".to_string(), metadata.name.clone());
            }
            
            let status = PodSandboxStatus {
                state: pod_sandbox.state,
                created_at: pod_sandbox.created_at,
                ..Default::default()
            };
            
            Ok(Response::new(PodSandboxStatusResponse {
                status: Some(status),
                info,
                containers_statuses: Vec::new(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            }))
        } else {
            Err(Status::not_found("Pod sandbox not found"))
        }
    }

    // 列出pod_sandbox
    async fn list_pod_sandbox(
        &self,
        _request: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let pod_sandboxes = self.pod_sandboxes.lock().await;
        let items = pod_sandboxes.values().cloned().collect();
        
        Ok(Response::new(ListPodSandboxResponse {
            items,
        }))
    }

    // 创建容器
    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        log::info!("CreateContainer called");
        let req = request.into_inner();
        let pod_sandbox_id = req.pod_sandbox_id.clone();
        let config = req.config.ok_or_else(|| Status::invalid_argument("Container config not specified"))?;
        
        let container_id = format!("container-{}", uuid::Uuid::new_v4());
        
        log::info!("Creating container with ID: {}", container_id);
        log::debug!("Container config: {:?}", config);

        // 构建容器配置
        let container_config = ContainerConfig {
            name: config.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| container_id.clone()),
            image: config.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
            command: config.command.clone(),
            args: config.args.clone(),
            env: config.envs.iter().map(|e| {
                let key = e.key.clone();
                let value = e.value.clone();
                (key, value)
            }).collect(),
            working_dir: if config.working_dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(&config.working_dir))
            },
            mounts: config.mounts.iter().map(|m| MountConfig {
                source: PathBuf::from(&m.host_path),
                destination: PathBuf::from(&m.container_path),
                read_only: m.readonly,
            }).collect(),
            labels: config.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            annotations: config.annotations.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            privileged: config.linux.as_ref().map(|l| l.security_context.as_ref().map(|s| s.privileged).unwrap_or(false)).unwrap_or(false),
            user: config.linux.as_ref().and_then(|l| l.security_context.as_ref()).and_then(|s| s.run_as_user.as_ref()).map(|u| u.value.to_string()),
            hostname: None,
            rootfs: self.config.root_dir.join("containers").join(&container_id).join("rootfs"),
        };
        
        // 调用runtime创建容器（在阻塞线程中执行）
        let runtime = self.runtime.clone();
        let runtime_container_id = container_id.clone();
        let container_config_clone = container_config.clone();
        let _created_id = tokio::task::spawn_blocking(move || {
            runtime.create_container(&container_config_clone)
        }).await
        .map_err(|e| Status::internal(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| Status::internal(format!("Failed to create container: {}", e)))?;
        
        // 创建容器元数据
        let container = Container {
            id: runtime_container_id.clone(),
            pod_sandbox_id: pod_sandbox_id.clone(),
            state: ContainerState::ContainerCreated as i32,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            labels: config.labels.clone(),
            metadata: config.metadata.clone(),
            image_ref: config.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
            ..Default::default()
        };
        
        // 存储容器信息到内存
        let mut containers = self.containers.lock().await;
        containers.insert(runtime_container_id.clone(), container.clone());
        log::info!("Container stored in memory, total containers: {}", containers.len());
        drop(containers);
        
        // 持久化容器状态
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.save_container(
            &runtime_container_id,
            &pod_sandbox_id,
            crate::runtime::ContainerStatus::Created,
            &config.image.as_ref().map(|i| i.image.clone()).unwrap_or_default(),
            &config.command,
            &config.labels,
            &config.annotations,
        ) {
            log::error!("Failed to persist container {}: {}", runtime_container_id, e);
        } else {
            log::info!("Container {} persisted to database", runtime_container_id);
        }
        
        Ok(Response::new(CreateContainerResponse {
            container_id: runtime_container_id,
        }))
    }

    // 启动容器
    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        
        log::info!("Starting container {}", container_id);

        // 检查容器是否存在
        let containers = self.containers.lock().await;
        if !containers.contains_key(&container_id) {
            return Err(Status::not_found("Container not found"));
        }
        drop(containers);

        // 调用runtime启动容器
        let runtime = self.runtime.clone();
        let container_id_clone = container_id.clone();
        tokio::task::spawn_blocking(move || {
            runtime.start_container(&container_id_clone)
        }).await
        .map_err(|e| Status::internal(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| Status::internal(format!("Failed to start container: {}", e)))?;

        // 更新容器状态到内存
        let mut containers = self.containers.lock().await;
        if let Some(container) = containers.get_mut(&container_id) {
            container.state = ContainerState::ContainerRunning as i32;
        }
        drop(containers);
        
        // 更新持久化状态
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.update_container_state(
            &container_id,
            crate::runtime::ContainerStatus::Running,
        ) {
            log::error!("Failed to update container {} state in database: {}", container_id, e);
        }
        
        log::info!("Container {} started", container_id);
        Ok(Response::new(StartContainerResponse { }))
    }

    // 停止容器
    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        let timeout = req.timeout as u32;
        
        log::info!("Stopping container {}", container_id);

        // 调用runtime停止容器
        let runtime = self.runtime.clone();
        let container_id_clone = container_id.clone();
        tokio::task::spawn_blocking(move || {
            runtime.stop_container(&container_id_clone, Some(timeout))
        }).await
        .map_err(|e| Status::internal(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| Status::internal(format!("Failed to stop container: {}", e)))?;

        // 更新容器状态到内存
        let mut containers = self.containers.lock().await;
        if let Some(container) = containers.get_mut(&container_id) {
            container.state = ContainerState::ContainerExited as i32;
        }
        drop(containers);
        
        // 更新持久化状态（标记为已停止）
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.update_container_state(
            &container_id,
            crate::runtime::ContainerStatus::Stopped(0),
        ) {
            log::error!("Failed to update container {} state in database: {}", container_id, e);
        }
        
        log::info!("Container {} stopped", container_id);
        Ok(Response::new(StopContainerResponse { }))
    }

    // 删除容器
    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        
        log::info!("Removing container {}", container_id);

        // 调用runtime删除容器
        let runtime = self.runtime.clone();
        let container_id_clone = container_id.clone();
        tokio::task::spawn_blocking(move || {
            runtime.remove_container(&container_id_clone)
        }).await
        .map_err(|e| Status::internal(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| Status::internal(format!("Failed to remove container: {}", e)))?;

        // 从内存中移除
        let mut containers = self.containers.lock().await;
        containers.remove(&container_id);
        drop(containers);
        
        // 从持久化存储中删除
        let mut persistence = self.persistence.lock().await;
        if let Err(e) = persistence.delete_container(&container_id) {
            log::error!("Failed to delete container {} from database: {}", container_id, e);
        } else {
            log::info!("Container {} removed from database", container_id);
        }
        
        log::info!("Container {} removed", container_id);
        Ok(Response::new(RemoveContainerResponse { }))
    }

    //重新打开容器日志
    async fn reopen_container_log(
        &self,
        _request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        // 实现重新打开容器日志的逻辑
        Ok(Response::new(ReopenContainerLogResponse { }))
    }

    //
    async fn attach(
        &self,
        _request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        // 实现 attach 的逻辑
        Ok(Response::new(AttachResponse {
            url: "unix:///var/run/crius/crius.sock".to_string(),
        }))
    }

    // 容器统计信息
    async fn container_stats(
        &self,
        request: Request<ContainerStatsRequest>,
    ) -> Result<Response<ContainerStatsResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        
        let containers = self.containers.lock().await;
        let _container = containers.get(&container_id)
            .ok_or_else(|| Status::not_found("Container not found"))?;
        
        // TODO: 从cgroup读取实际统计信息
        Ok(Response::new(ContainerStatsResponse {
            stats: None,
        }))
    }

    // 容器列表统计信息
    async fn list_container_stats(
        &self,
        _request: Request<ListContainerStatsRequest>,
    ) -> Result<Response<ListContainerStatsResponse>, Status> {
        // TODO: 实现完整的统计列表
        Ok(Response::new(ListContainerStatsResponse {
            stats: Vec::new(),
        }))
    }

    // pod沙箱统计信息
    async fn pod_sandbox_stats(
        &self,
        request: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        let req = request.into_inner();
        let pod_id = req.pod_sandbox_id;
        
        let pods = self.pod_sandboxes.lock().await;
        let _pod = pods.get(&pod_id)
            .ok_or_else(|| Status::not_found("Pod sandbox not found"))?;
        
        // TODO: 从cgroup读取实际统计信息
        Ok(Response::new(PodSandboxStatsResponse {
            stats: None,
        }))
    }

    // pod沙箱列表统计信息
    async fn list_pod_sandbox_stats(
        &self,
        _request: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        // TODO: 实现完整的统计列表
        Ok(Response::new(ListPodSandboxStatsResponse {
            stats: Vec::new(),
        }))
    }

    // 更新运行时配置
    async fn update_runtime_config(
        &self,
        _request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        // 实现 update_runtime_config 的逻辑
        Ok(Response::new(UpdateRuntimeConfigResponse { }))
    }

    //
    async fn checkpoint_container(
        &self,
        _request: Request<CheckpointContainerRequest>,
    ) -> Result<Response<CheckpointContainerResponse>, Status> {
        // 实现 checkpoint_container 的逻辑
        Ok(Response::new(CheckpointContainerResponse { }))
    }

    type GetContainerEventsStream = ReceiverStream<Result<ContainerEventResponse, Status>>;

    //
    async fn get_container_events(
        &self,
        _request: Request<GetEventsRequest>,
    ) -> Result<Response<Self::GetContainerEventsStream>, Status> {
        log::info!("Get container events");
        
        // 创建channel用于事件流
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let containers = self.containers.clone();
        let pods = self.pod_sandboxes.clone();
        
        // 在后台任务中读取当前状态并发送
        tokio::spawn(async move {
            // 获取当前容器状态
            let containers_map = containers.lock().await;
            for (id, container) in containers_map.iter() {
                let event = ContainerEventResponse {
                    container_id: id.clone(),
                    container_event_type: 0, // CONTAINER_CREATED_EVENT
                    created_at: container.created_at,
                    pod_sandbox_status: None,
                    containers_statuses: vec![crate::proto::runtime::v1::ContainerStatus {
                        id: id.clone(),
                        metadata: container.metadata.clone(),
                        state: container.state,
                        created_at: container.created_at,
                        ..Default::default()
                    }],
                };
                
                if let Err(e) = tx.send(Ok(event)).await {
                    log::error!("Failed to send container event: {}", e);
                    break;
                }
            }
            drop(containers_map);
            
            // 获取当前Pod状态
            let pods_map = pods.lock().await;
            for (id, pod) in pods_map.iter() {
                let event = ContainerEventResponse {
                    container_id: id.clone(),
                    container_event_type: 2, // POD_SANDBOX_CREATED_EVENT
                    created_at: pod.created_at,
                    pod_sandbox_status: Some(crate::proto::runtime::v1::PodSandboxStatus {
                        id: id.clone(),
                        state: pod.state,
                        created_at: pod.created_at,
                        ..Default::default()
                    }),
                    containers_statuses: vec![],
                };
                
                if let Err(e) = tx.send(Ok(event)).await {
                    log::error!("Failed to send pod event: {}", e);
                    break;
                }
            }
        });
        
        let stream = ReceiverStream::new(rx);
        Ok(Response::new(stream))
    }

    //
    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        // 实现 list_metric_descriptors 的逻辑
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: Vec::new(),
        }))
    }

    //
    async fn list_pod_sandbox_metrics(
        &self,
        _request: Request<ListPodSandboxMetricsRequest>,
    ) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
        // 实现 list_pod_sandbox_metrics 的逻辑
        Ok(Response::new(ListPodSandboxMetricsResponse {
            pod_metrics: Vec::new(),
        }))
    }

    //
    async fn runtime_config(
        &self,
        _request: Request<RuntimeConfigRequest>,
    ) -> Result<Response<RuntimeConfigResponse>, Status> {
        // 返回当前运行时配置
        let config = RuntimeConfigResponse {
            linux: Some(crate::proto::runtime::v1::LinuxRuntimeConfiguration {
                ..Default::default()
            }),
            ..Default::default()
        };
        
        Ok(Response::new(config))
    }

    async fn update_container_resources(
        &self,
        request: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id;
        
        // 检查容器是否存在
        let containers = self.containers.lock().await;
        let _container = containers.get(&container_id)
            .ok_or_else(|| Status::not_found("Container not found"))?;
        drop(containers);
        
        // 获取资源限制
        let linux = req.linux;
        let _windows = req.windows;
        
        if let Some(resources) = linux {
            log::info!(
                "Updating container {} resources: CPU shares={}, Memory limit={}",
                container_id,
                resources.cpu_shares,
                resources.memory_limit_in_bytes
            );
            
            // TODO: 通过runc update命令更新cgroups资源限制
            // 这需要调用runc update --cpus-shares X --memory-limit Y <container_id>
        }
        
        Ok(Response::new(UpdateContainerResourcesResponse {}))
    }
}
