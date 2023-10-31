use serde_json::json;

mod errors;
mod fb_access_token;
mod jwt;

use fb_access_token::ServiceAccount;

// TODO(adnanjpg): take from the file that path is at
// `GOOGLE_APPLICATION_CREDENTIALS` env variable
const PROJECT_ID: &str = "remon-mobile-b0c23";

const SERVICE_KEY_FILE_NAME: &str = ".remon-mobile-fcm-creds.json";

pub async fn access_token() -> Result<String, ()> {
    let scopes = vec!["https://www.googleapis.com/auth/firebase.messaging"];
    let key_path = SERVICE_KEY_FILE_NAME;

    let mut service_account = ServiceAccount::from_file(key_path, scopes);
    let access_token = match service_account.access_token().await {
        Ok(access_token) => access_token,
        Err(_) => return Err(()),
    };

    let token_no_bearer = access_token.split(" ").collect::<Vec<&str>>()[1];

    Ok(token_no_bearer.to_string())
}

pub async fn send_notification_to(device_id: &str) -> Result<bool, String> {
    send_notification_to_multi(&vec![device_id]).await
}

pub async fn send_notification_to_multi(device_ids: &Vec<&str>) -> Result<bool, String> {
    // using reqwest, send a request to fcm v1, with the
    // title "test title", and the message "test message"

    // https://firebase.google.com/docs/reference/fcm/rest/v1/projects.messages/send

    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        PROJECT_ID
    );

    let body = json!({
        "message": {
            "token": device_ids[0],
            "notification": {
                "title": format!("test title {}", chrono::Utc::now().timestamp()),
                "body": format!("test body {}", chrono::Utc::now().timestamp()),
            }
        }
    });

    let tkn = match access_token().await {
        Ok(tkn) => tkn,
        Err(_) => return Err("could not get access token".to_string()),
    };

    let client = reqwest::Client::new();
    let req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .bearer_auth(tkn)
        .body(body.to_string());

    let res = req.send().await;

    match res {
        Ok(res) => {
            let stat = res.status();
            let suc = stat.is_success();

            Ok(suc)
        }
        Err(err) => {
            println!("err: {:?}", err);
            Err(err.to_string())
        }
    }
}
