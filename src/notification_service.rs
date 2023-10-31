use serde_json::json;

mod errors;
mod fb_access_token;
mod jwt;

use fb_access_token::ServiceAccount;

const SERVICE_KEY_FILE_NAME: &str = ".remon-mobile-fcm-creds.json";

fn read_service_key_file() -> Result<String, String> {
    let key_path = SERVICE_KEY_FILE_NAME;

    let private_key_content = match std::fs::read(key_path) {
        Ok(content) => content,
        Err(err) => return Err(err.to_string()),
    };

    Ok(String::from_utf8(private_key_content).unwrap())
}

fn read_service_key_file_json() -> Result<serde_json::Value, String> {
    let file_content = match read_service_key_file() {
        Ok(content) => content,
        Err(err) => return Err(err),
    };

    let json_content: serde_json::Value = match serde_json::from_str(&file_content) {
        Ok(json) => json,
        Err(err) => return Err(err.to_string()),
    };

    Ok(json_content)
}

fn get_project_id() -> Result<String, String> {
    let json_content = match read_service_key_file_json() {
        Ok(json) => json,
        Err(err) => return Err(err),
    };

    let project_id = match json_content["project_id"].as_str() {
        Some(project_id) => project_id,
        None => return Err("could not get project_id".to_string()),
    };

    Ok(project_id.to_string())
}

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
    let project_id = match get_project_id() {
        Ok(project_id) => project_id,
        Err(err) => return Err(err),
    };

    // https://firebase.google.com/docs/reference/fcm/rest/v1/projects.messages/send
    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        project_id
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
