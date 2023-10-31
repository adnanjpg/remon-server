use gauth::serv_account::ServiceAccount;
use serde_json::json;

use std::fs::File;
use std::io::Read;

// TODO(adnanjpg): take from the file that path is at
// `GOOGLE_APPLICATION_CREDENTIALS` env variable
const PROJECT_ID: &str = "remon-mobile-b0c23";

const SERVICE_KEY_FILE_NAME: &str = ".remon-mobile-fcm-creds.json";

pub fn access_token() {
    let scopes = vec!["https://www.googleapis.com/auth/firebase.messaging"];
    let key_path = SERVICE_KEY_FILE_NAME;

    let mut service_account = ServiceAccount::from_file(key_path, scopes);
    let access_token = service_account.access_token().unwrap();

    println!("access token {}:", access_token);
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
    // let tkn = "ya29.a0AfB_byBce3y09A-g1PsKsS8x0XoQsG1F0l5FnxieHxfIy1veNVzvnRPGdyZ1iBCk6QWno9N0VkSiw9s4ZwwjaoGhNLhs-405gv_4osUMrIuY7-ZYZ_lbeeIgOaylr21tWNpg7V-90kBiA2yNZ8Zebh0xisS7lfTy1NYCaCgYKAbsSARASFQGOcNnCi6tc4wuYL3XDfVlmqz0Ivw0171".to_string();
    let tkn = "ya29.c.c0AY_VpZinPsGkzwIiCyU95-gBLlIt5kJujtjTvsxfpf5f0XrOLSpehcqsGb5VTMtUlSsAkKxxlSNd5kH32lqsVaIEg7g_R8RB-ykEBhVg00OpijPcGldT7y5iu1YBTckbrnADVQZ9hziQBtKjtMRumg3FMQOdtywlIqOKf_DAEjT9U7nmYJZI3YUK3TOlfvXBd61RBP0AhLnbi6_yn4vlkWQHv_KY_dJxW664iTVg3jGuTOdrnyShh5-3XD07DR65NdiekLJlXeFz9MKLUoBDgWieBPJOX0mBATx4fV8LX4wCE32wgSUeZPXputXKPfVQbFOjb6R1Lg0Hf2QblDUOXhZsDw2Hk5prVeo3MX_DL-WGKtMx1DS8C4sH384Au-Sehd83gpRf4O_6oimM3B4B1Yy6iZ9d9zUl6F8fMMx9sVO4eIOlodFgxkQnYk402aZeBoQFOfqsdsYk62rhi-McgMks0wcB_qupexQgt8UBcjtOFeuOVMzOBbYZMmpdjipzqqRsmWQnp_z5xcZX6YVzVnq2wyYFInkmx9SgeaUwqX7l_jUO67tzUyw-1pU9nmi1r7QxeklxuM7Uq5k_tzzmouVXSzIWWi0BUel5tfUU_tjVjXxqSnzUy8_d5tZ1UZ20iIXVgIV19Vh9t4uVSUdZaS1Zv5cp-ssftwndSiMBoBV475iw2WIksO3za70cgX8gvlxdgVWRugwxypR_6aJXaOVMYqfguzs6w9eFtW150IBgbW6lqr6R6do1cexImbt-9JOsXIpv1W5fv5n3UWpyRynjsXbBRUQxvpf4Xjv1ey2pFwkuyyqtmer0WQsqhOB4qwsb5nY-kdpIzl-bRyvrMY5_4mQ9kVoZpbOQ5mQi71Z1i3vF-oJsg7-Zl1Yonah_ap0wliwIq_2ObtJscfsQ-r1tqWQjlc62kR5qkl3ydx3VOieyoQ8caYS-q5IQpMcgFSZif1y5Rc4J_3Z72XMWsWh6y3WhW_J3rpt3Mvmavn-1l9Uckh1u-on:".to_string();

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
