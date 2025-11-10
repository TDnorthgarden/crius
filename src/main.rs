use std::path::PathBuf;
use std::net::SocketAddr;

use clap::Parser;
use tonic::transport::Server;
use tracing_subscriber::{fmt, EnvFilter};
use tracing::{debug, info};
use anyhow::Error;

use crate::server::{RuntimeConfig, RuntimeServiceImpl};
use crate::image::ImageServiceImpl;

mod server;
mod image;
mod proto;
mod error;

/// crius - OCI-based implementation of Kubernetes Container Runtime Interface
#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    /// Path to the configuration file
    #[clap(short, long, default_value = "/etc/crius/crius.conf")]
    config: PathBuf,

    /// Enable debug logging
    #[clap(short, long)]
    debug: bool,

    /// Log file path
    #[clap(short, long)]
    log: Option<PathBuf>,

    /// Listen address
    #[clap(long, default_value = "0.0.0.0:10000")]
    listen: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // 初始化日志
    init_logging()?;
    
    // 解析命令行参数
    let args = Args::parse();
    
    // 创建运行时配置
    let runtime_config = RuntimeConfig {
        root_dir: PathBuf::from("/var/lib/crius"),
        runtime: "runc".to_string(),
        runtime_root: PathBuf::from("/var/run/runc"),
        log_dir: PathBuf::from("/var/log/crius"),
    };

    // 创建服务实例
    let runtime_service = RuntimeServiceImpl::new(runtime_config.clone());
    let image_service = ImageServiceImpl::new(runtime_config.root_dir.join("storage"))?;

    // 加载本地镜像
    image_service.load_local_images().await?;

    // 创建gRPC服务器
    let addr: SocketAddr = args.listen.parse()?;
    
    info!("Starting crius gRPC server on {}", addr);
    debug!("Using configuration: {:?}", runtime_config);
    
    // 启动CRI服务
    Server::builder()
        .add_service(
            crate::proto::runtime::v1::runtime_service_server::RuntimeServiceServer::new(
                runtime_service,
            ),
        )
        .add_service(
            crate::proto::runtime::v1::image_service_server::ImageServiceServer::new(
                image_service,
            ),
        )
        .serve(addr)
        .await?;
    
    Ok(())
}

fn init_logging() -> Result<(), Error> {
       
    let filter = EnvFilter::from_default_env()
        .add_directive("crius=info".parse()?)
        .add_directive("tower_http=info".parse()?);
    
    fmt()
        .with_file(true)
        .with_line_number(true)
        .with_writer(std::io::stderr)
        .init();
    
    Ok(())
}
