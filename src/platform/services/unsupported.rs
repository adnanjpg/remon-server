/// Fallback for platforms where no service manager is implemented yet
/// (FreeBSD, macOS, etc.).
use async_trait::async_trait;

use super::{Service, ServiceError, ServiceFilter, ServiceManager};

pub struct UnsupportedManager;

#[async_trait]
impl ServiceManager for UnsupportedManager {
    async fn list(&self, _filter: ServiceFilter) -> Result<Vec<Service>, ServiceError> {
        Err(ServiceError::NotSupported)
    }

    async fn get(&self, name: &str) -> Result<Service, ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    async fn start(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    async fn stop(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    async fn restart(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    async fn enable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    async fn disable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }
}
