use log::error;
use serde_derive::{Deserialize, Serialize};

use fcm;

use crate::persistence::notification_logs::{
    insert_notification_log, NotificationLog, NotificationType,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct NotificationMessage {
    pub title: String,
    pub body: String,
}

async fn send_notification_to(
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
            // let stat = res.status();
            // let suc = stat.is_success();
            let suc_count = res.success;

            let is_suc = suc_count == Some(sent_notification_count);

            let sent_at = chrono::Utc::now().timestamp_millis();
            let add_res =
                add_notification_log(device_id, fcm_token, message, notification_type, &sent_at)
                    .await;

            if !add_res {
                return Err("failed to insert notifiaction log".to_string());
            };

            Ok(is_suc)
        }
        Err(err) => {
            println!("err: {:?}", err);
            Err(err.to_string())
        }
    }
}

pub async fn add_notification_log(
    device_id: &str,
    fcm_token: &str,
    message: &NotificationMessage,
    notification_type: &NotificationType,
    sent_at: &i64,
) -> bool {
    let not_log = NotificationLog {
        id: -1,
        notification_type: notification_type.to_owned(),
        device_id: device_id.to_string(),
        fcm_token: fcm_token.to_string(),
        sent_at: sent_at.to_owned(),
        title: message.title.to_string(),
        body: message.body.to_string(),
    };

    let ret = match insert_notification_log(&not_log).await {
        Ok(_) => true,
        Err(error) => {
            error!("{}", error);

            return false;
        }
    };

    ret
}

pub async fn send_notification_to_multi(
    device_ids_and_tokens: &Vec<(&str, &str)>,
    message: &NotificationMessage,
    notification_type: &NotificationType,
) -> Result<bool, String> {
    let mut results = Vec::new();

    for dev in device_ids_and_tokens {
        let res = send_notification_to(dev.0, dev.1, &message, notification_type).await;

        match res {
            Ok(res) => results.push(res),
            Err(err) => return Err(err),
        }
    }

    Ok(results.iter().all(|&x| x))
}

pub async fn send_notification_to_single(
    device_id: &str,
    fcm_token: &str,
    message: &NotificationMessage,
    notification_type: &NotificationType,
) -> Result<bool, String> {
    send_notification_to_multi(&vec![(device_id, fcm_token)], &message, notification_type).await
}
