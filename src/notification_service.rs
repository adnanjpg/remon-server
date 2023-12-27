use gauth::serv_account::ServiceAccount;
use serde_json::json;

// reads the service key file name from the environment
// variable GOOGLE_APPLICATION_CREDENTIALS
fn get_service_key_file_name() -> Result<String, String> {
    let key_path = match dotenv::var("GOOGLE_APPLICATION_CREDENTIALS") {
        Ok(key_path) => key_path,
        Err(err) => return Err(err.to_string()),
    };

    Ok(key_path)
}

fn read_service_key_file() -> Result<String, String> {
    let key_path = get_service_key_file_name()?;

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

pub async fn access_token() -> Result<String, String> {
    let scopes = vec!["https://www.googleapis.com/auth/firebase.messaging"];
    let key_path = get_service_key_file_name()?;

    let mut service_account = ServiceAccount::from_file(&key_path, scopes);
    let access_token = match service_account.access_token().await {
        Ok(access_token) => access_token,
        Err(err) => return Err(err.to_string()),
    };

    let token_no_bearer = access_token.split(" ").collect::<Vec<&str>>()[1];

    Ok(token_no_bearer.to_string())
}

async fn get_auth_token() -> Result<String, String> {
    let tkn = match access_token().await {
        Ok(tkn) => tkn,
        Err(_) => return Err("could not get access token".to_string()),
    };

    Ok(tkn)
}

async fn send_notification_to(
    device_id: &str,
    auth_token: &str,
    project_id: &str,
) -> Result<bool, String> {
    // https://firebase.google.com/docs/reference/fcm/rest/v1/projects.messages/send
    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        project_id
    );

    let body = json!({
        "message": {
            "token": device_id,
            "notification": {
                "title": format!("test title {}", chrono::Utc::now().timestamp()),
                "body": format!("test body {}", chrono::Utc::now().timestamp()),
            }
        }
    });

    let client = reqwest::Client::new();
    let req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .bearer_auth(auth_token)
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

pub async fn send_notification_to_multi(device_ids: &Vec<&str>) -> Result<bool, String> {
    let project_id = match get_project_id() {
        Ok(project_id) => project_id,
        Err(err) => return Err(err),
    };

    let tkn = match get_auth_token().await {
        Ok(tkn) => tkn,
        Err(err) => return Err(err),
    };

    let mut results = Vec::new();

    for device_id in device_ids {
        let res = send_notification_to(device_id, &tkn, &project_id).await;

        match res {
            Ok(res) => results.push(res),
            Err(err) => return Err(err),
        }
    }

    Ok(results.iter().all(|&x| x))
}

#[allow(dead_code)]
pub async fn send_notification_to_single(device_id: &str) -> Result<bool, String> {
    send_notification_to_multi(&vec![device_id]).await
}
