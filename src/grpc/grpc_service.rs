use log::{debug, error, info};
use tonic::{transport::Server, Request, Response, Status};
use tonic_reflection::server as ReflectionServer;

use crate::{
    logs::persistence::{insert_app_log, AppLog, LogLevel, NotificationType},
    monitor::persistence::fetch_monitor_configs,
    notification_service,
};

use remonproto::{
    notification_service_server::{
        NotificationService as NotificationServiceImpl, NotificationServiceServer,
    },
    LogRequest, LogResponse, NotificationRequest, NotificationResponse,
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

        debug!(
            "Received notification: {} - {}",
            request.title, request.body
        );

        // TODO(@isaidsari): this part is not complete
        // it needs to think more about it
        let configs = fetch_monitor_configs().await.unwrap_or_else(|e| {
            error!("failed to fetch monitor configs: {}", e);
            vec![]
        });

        if configs.is_empty() {
            // return Ok(Response::new(NotificationResponse {
            //     success: false,
            //     message: Some("No monitor configs found".to_string()),
            // }));
            return Err(Status::not_found("No monitor configs found"));
        };

        let config = match configs.last() {
            Some(config) => config,
            None => {
                return Err(Status::not_found("No monitor configs found"));
            }
        };

        debug!("gRPC request: {:?}", request);

        let result = notification_service::send_notification_to_single(
            &config.device_id,
            &config.fcm_token,
            &notification_service::NotificationMessage {
                title: request.title,
                body: request.body,
            },
            &NotificationType::StatusLimitsExceeding,
        )
        .await;

        // let send_to = configs
        //     .iter()
        //     .map(|config| (config.device_id.as_str(), config.fcm_token.as_str()))
        //     .collect::<Vec<(&str, &str)>>();
        // let result = notification_service::send_notification_to_multi(
        //     &send_to,
        //     &notification_service::NotificationMessage {
        //         title: request.title,
        //         body: request.body,
        //     },
        //     &NotificationType::StatusLimitsExceeding,
        // )
        // .await;

        match result {
            Ok(_) => {
                debug!("Notification sent: {:?}", result);
                Ok(Response::new(NotificationResponse {
                    success: true,
                    message: Some("Notification sent".to_string()),
                }))
            }
            Err(e) => {
                debug!("Failed to send notification: {}", e);
                Err(Status::internal(format!(
                    "Failed to send notification: {}",
                    e
                )))
            }
        }
    }

    async fn log(&self, request: Request<LogRequest>) -> Result<Response<LogResponse>, Status> {
        let log = request.into_inner();

        debug!("Received log message: {}", log.message);

        let app_log = AppLog {
            id: -1,
            log_level: LogLevel::from_string(&log.level),
            app_id: "gRPC".to_string(), // TODO(isaidsari):
            logged_at: chrono::Utc::now().timestamp(),
            message: log.message,
            target: log.target,
        };

        match insert_app_log(&app_log).await {
            Ok(_) => {
                debug!("Log received: {:?}", app_log);
                Ok(Response::new(LogResponse {
                    success: true,
                    message: Some("Log received".to_string()),
                }))
            }
            Err(e) => {
                debug!("Failed to insert log: {}", e);
                Err(Status::internal(format!("Failed to insert log: {}", e)))
            }
        }
    }
}

pub async fn init() -> Result<(), Box<dyn std::error::Error>> {
    let notification_service = NotificationServiceServer::new(NotificationService::default());

    // use reflection to expose the service
    let reflection_service = ReflectionServer::Builder::configure()
        .register_encoded_file_descriptor_set(remonproto::FILE_DESCRIPTOR_SET)
        .build()
        .unwrap();
    info!("gRPC service listening on {}", DEFAULT_ADDR);

    tokio::spawn(async move {
        Server::builder()
            .add_service(notification_service)
            .add_service(reflection_service)
            .serve(DEFAULT_ADDR.parse().unwrap())
            .await
            .unwrap();
    });

    Ok(())
}
