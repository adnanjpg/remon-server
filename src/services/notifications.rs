use log::error;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use fcm;

use crate::storage::repositories::LogRepository;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationType {
    General,
    Alert,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NotificationMessage {
    pub title: String,
    pub body: String,
}

async fn send_notification_to(
    db: &SqlitePool,
    device_id: &str,
    fcm_token: &str,
    message: &NotificationMessage,
    notification_type: &NotificationType,
) -> Result<bool, String> {
    let builder = fcm::Message {
        data: None,
        notification: Some(fcm::Notification {
            title: Some(message.title.to_owned()),
            body: Some(message.body.to_owned()),
            ..Default::default()
        }),
        target: fcm::Target::Token(fcm_token.to_owned()),
        android: Some(fcm::AndroidConfig {
            priority: Some(fcm::AndroidMessagePriority::High),
            ..Default::default()
        }),
        apns: None,
        webpush: None,
        fcm_options: None,
    };

    let client = fcm::Client::new();
    let response = client.send(builder).await;
    let sent_notification_count = 1;

    match response {
        Ok(res) => {
            let is_suc = res.success == Some(sent_notification_count);

            // Log notification to database
            let log_repo = LogRepository::new(db.clone());
            let type_str = match notification_type {
                NotificationType::General => "general",
                NotificationType::Alert => "alert",
            };
            if let Err(e) = log_repo
                .insert(0, type_str, device_id, &format!("{}: {}", message.title, message.body))
                .await
            {
                error!("Failed to log notification: {}", e);
            }

            Ok(is_suc)
        }
        Err(err) => {
            error!("FCM error: {:?}", err);
            Err(err.to_string())
        }
    }
}

pub async fn send_notification_to_multi(
    db: &SqlitePool,
    device_ids_and_tokens: &[(&str, &str)],
    message: &NotificationMessage,
    notification_type: &NotificationType,
) -> Result<bool, String> {
    let mut results = Vec::new();

    for (device_id, fcm_token) in device_ids_and_tokens {
        let res = send_notification_to(db, device_id, fcm_token, message, notification_type).await;
        match res {
            Ok(res) => results.push(res),
            Err(err) => return Err(err),
        }
    }

    Ok(results.iter().all(|&x| x))
}

pub async fn send_notification_to_single(
    db: &SqlitePool,
    device_id: &str,
    fcm_token: &str,
    message: &NotificationMessage,
    notification_type: &NotificationType,
) -> Result<bool, String> {
    send_notification_to_multi(db, &[(device_id, fcm_token)], message, notification_type).await
}