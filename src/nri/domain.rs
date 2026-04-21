use crate::nri_proto::api as nri_api;

#[derive(Debug, Clone, Default)]
pub struct RuntimeSnapshot {
    pub pods: Vec<nri_api::PodSandbox>,
    pub containers: Vec<nri_api::Container>,
}
