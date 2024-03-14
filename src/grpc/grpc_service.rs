use log::{error, info};
use tonic::{transport::Server, Request, Response, Status};
use tonic_reflection::server as ReflectionServer;

use crate::{
    monitor::persistence::fetch_monitor_configs, notification_service,
    persistence::notification_logs::NotificationType,
};

use remonproto::{
    notification_service_server::{
        NotificationService as NotificationServiceImpl, NotificationServiceServer,
    },
    NotificationRequest, NotificationResponse,
};

pub mod remonproto {
    tonic::include_proto!("remonproto");

    pub(super) const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("remonproto_descriptor");
}

const DEFAULT_ADDR: &str = "[::1]:50051";

#[derive(Default, Debug)]
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

        let configs = fetch_monitor_configs().await.unwrap_or_else(|e| {
            error!("failed to fetch monitor configs: {}", e);
            vec![]
        });

        let res = notification_service::send_notification_to_single(
            &configs[0].device_id,
            &configs[0].fcm_token,
            &notification_service::NotificationMessage {
                title: request.title,
                body: request.body,
            },
            &NotificationType::StatusLimitsExceeding,
        )
        .await;

        info!("Notification sent: {:?}", res);

        let response = if res.is_ok() {
            NotificationResponse {
                success: true,
                message: Option::from("Notification sent successfully".to_string()),
            }
        } else {
            NotificationResponse {
                success: false,
                message: Option::from("Failed to send notification".to_string()),
            }
        };

        // return the response
        Ok(Response::new(response))
    }
}

pub async fn init() -> Result<(), Box<dyn std::error::Error>> {
    let notification_service = NotificationService::default();

    // use reflection to expose the service
    let reflection_service = ReflectionServer::Builder::configure()
        .register_encoded_file_descriptor_set(remonproto::FILE_DESCRIPTOR_SET)
        .build()
        .unwrap();

    info!("gRPC service listening on {}", DEFAULT_ADDR);

    tokio::spawn(async move {
        Server::builder()
            .add_service(NotificationServiceServer::new(notification_service))
            .add_service(reflection_service)
            .serve(DEFAULT_ADDR.parse().unwrap())
            .await
            .unwrap();
    });

    Ok(())
}
