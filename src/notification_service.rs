use serde_json::json;

use std::fs::File;
use std::io::Read;

// TODO(adnanjpg): take from the file that path is at
// `GOOGLE_APPLICATION_CREDENTIALS` env variable
const PROJECT_ID: &str = "remon-mobile-b0c23";

const FILE_NAME: &str = ".remon-mobile-fcm-creds.json";

fn get_file_contents(file_name: &str) -> Result<String, ()> {
    let mut cred_file = match File::open(file_name) {
        Ok(file) => file,
        Err(err) => {
            println!("err: {:?}", err);
            return Err(());
        }
    };

    // Create a buffer to store the file's contents
    let mut contents = String::new();

    // Read the file's contents into the buffer
    match cred_file.read_to_string(&mut contents) {
        Ok(_) => (),
        Err(err) => {
            println!("err: {:?}", err);
            return Err(());
        }
    }

    Ok(contents)
}

fn get_file_json(file_name: &str) -> Result<serde_json::Value, ()> {
    let contents = get_file_contents(file_name).unwrap();

    let json: serde_json::Value = serde_json::from_str(&contents).unwrap();

    Ok(json)
}

async fn get_k() -> Result<String, ()> {
    let cred_file_json = get_file_json(FILE_NAME).unwrap();

    // the kid field in the header, specify your service account's private key ID. You can find this value in the private_key_id field of your service account JSON file
    let private_key = cred_file_json["private_key"].as_str().unwrap();

    // {"alg":"RS256","typ":"JWT", "kid":"370ab79b4513eb9bad7c9bd16a95cb76b5b2a56a"}
    let mut headers: jsonwebtoken::Header =
        jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    headers.typ = Some("JWT".to_owned());
    headers.kid = Some(private_key.to_owned());

    let client_email = cred_file_json["client_email"].as_str().unwrap();
    // iat is the current time in seconds since the epoch
    // and exp is 3600 seconds after that
    let iat = chrono::Utc::now().timestamp();
    let exp = iat + 3600;
    let aud = "https://firebase.google.com/";

    let claims = json!({
        "iss": client_email,
        "sub": client_email,
        "aud": aud,
        "iat": iat,
        "exp": exp
    });

    //     {
    //   "alg": "RS256",
    //   "typ": "JWT",
    //   "kid": "abcdef1234567890"
    // }
    // .
    // {
    //   "iss": "123456-compute@developer.gserviceaccount.com",
    //   "sub": "123456-compute@developer.gserviceaccount.com",
    //   "aud": "https://firestore.googleapis.com/",
    //   "iat": 1511900000,
    //   "exp": 1511903600
    // }
    let token = jsonwebtoken::encode(
        // use `headers`    as the header
        &headers,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes()).unwrap(),
    );

    Ok(token.unwrap())
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

    // let tkn = get_k().await.unwrap();
    let tkn = "ya29.a0AfB_byBce3y09A-g1PsKsS8x0XoQsG1F0l5FnxieHxfIy1veNVzvnRPGdyZ1iBCk6QWno9N0VkSiw9s4ZwwjaoGhNLhs-405gv_4osUMrIuY7-ZYZ_lbeeIgOaylr21tWNpg7V-90kBiA2yNZ8Zebh0xisS7lfTy1NYCaCgYKAbsSARASFQGOcNnCi6tc4wuYL3XDfVlmqz0Ivw0171".to_string();

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
            let body = res.text().await.unwrap();
            println!("body: {:?}", body);
            println!("stat: {:?}", stat);

            let suc = stat.is_success();

            Ok(suc)
        }
        Err(err) => {
            println!("err: {:?}", err);
            Err(err.to_string())
        }
    }
}
