use log::{info};
use tonic::{transport::Server, Request, Response, Status};

pub mod remonproto {
    tonic::include_proto!("remonproto");

    pub(super) const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("remonproto_descriptor");
}

use remonproto::{
    notification_service_server::{NotificationService as NotificationServiceImpl, NotificationServiceServer},
    NotificationRequest, NotificationResponse,
};

#[derive(Default)]
pub struct NotificationService;

#[tonic::async_trait]
impl NotificationServiceImpl for NotificationService {
    async fn send_notification(
        &self,
        request: Request<NotificationRequest>,
    ) -> Result<Response<NotificationResponse>, Status> {
        let request = request.into_inner();

        info!(
            "Received notification: {} - {}",
            request.title, request.body
        );

        // TODO(isaidsari): send notification here

        let response = remonproto::NotificationResponse {
            message: "Notification received".into(),
        };

        // return the response
        Ok(Response::new(response))
    }
}

pub async fn init() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let notification_service = NotificationService::default();

    // use reflection to expose the service
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(remonproto::FILE_DESCRIPTOR_SET)
        .build()
        .unwrap();

    info!("gRPC service listening on {}", addr);

    Server::builder()
        .add_service(NotificationServiceServer::new(notification_service))
        .add_service(reflection_service)
        .serve(addr)
        .await?;

    Ok(())
}
