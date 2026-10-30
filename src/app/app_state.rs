use crate::{
    domain::EverythingConfig, platform::windows::shell_integration::ShellIntegrationStatus,
};

#[derive(Default)]
pub(super) struct EverythingState {
    pub(super) config: EverythingConfig,
    pub(super) generation: u64,
    pub(super) status: String,
    pub(super) busy: bool,
    pub(super) folder_sizes_indexed: Option<bool>,
}

#[derive(Default)]
pub(super) struct IntegrationState {
    pub(super) folder_status: Option<ShellIntegrationStatus>,
    pub(super) win_e_status: Option<ShellIntegrationStatus>,
    pub(super) generation: u64,
    pub(super) busy: bool,
    pub(super) folder_error: Option<String>,
    pub(super) win_e_error: Option<String>,
}
