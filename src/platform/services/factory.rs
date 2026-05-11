use std::sync::Arc;

use log::info;
#[cfg(not(target_os = "windows"))]
use log::warn;

use crate::platform::init::InitSystem;

use super::ServiceManager;

/// Instantiate the appropriate `ServiceManager` for the detected init system.
/// Linux gets a full systemd or OpenRC backend; Windows uses the SCM bridge
/// via PowerShell shell-out; everything else falls back to
/// `UnsupportedManager` which returns `NotSupported` for every call.
pub async fn create(init: &InitSystem) -> Arc<dyn ServiceManager> {
    #[cfg(target_os = "linux")]
    {
        match init {
            InitSystem::Systemd => {
                info!("Service backend: systemd");
                return Arc::new(super::systemd::SystemdManager);
            }
            InitSystem::OpenRc => {
                info!("Service backend: OpenRC");
                return Arc::new(super::openrc::OpenRcManager);
            }
            _ => {
                warn!(
                    "Unknown Linux init system ({:?}); service management unavailable",
                    init
                );
                return Arc::new(super::unsupported::UnsupportedManager);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = init;
        info!("Service backend: Windows SCM (via PowerShell)");
        return Arc::new(super::windows_scm::WindowsScmManager);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = init;
        warn!("Service management not supported on this platform");
        Arc::new(super::unsupported::UnsupportedManager)
    }
}
