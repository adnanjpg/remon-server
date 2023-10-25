use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use serde::Serialize;

use std::convert::Infallible;
use std::net::SocketAddr;

mod get_otp;

use serde_json;

use crate::get_otp::TOTP_KEY;

#[derive(serde::Deserialize, Serialize)] // Derive Deserialize and Serialize for your struct
struct ValidateOtpData {
    token: String,
}

async fn req_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match req.uri().path() {
        "/hello" => {
            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from("Hello World\r\n"))
                .unwrap();
            Ok(response)
        }
        "/get-otp-qr" => {
            // TODO(adnanjpg): take from request
            let user_name = "adnanjpg";

            let qr_code = get_otp::generate_otp_qr_code(user_name.to_owned());

            get_otp::outputqr(&qr_code.to_string()).unwrap();

            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(""))
                .unwrap();

            Ok(response)
        }
        "/validate-totp" => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            // Parse the request body as JSON
            let request_data: Result<ValidateOtpData, serde_json::Error> =
                serde_json::from_str(&body_str);

            let is_matching = get_otp::check_totp_match(&request_data.unwrap().token, TOTP_KEY);

            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(is_matching.to_string()))
                .unwrap();
            Ok(response)
        }
        _ => {
            let response = Response::builder()
                .status(404)
                .header("Content-Type", "text/plain")
                .body(Body::from("Not Found\r\n"))
                .unwrap();
            Ok(response)
        }
    }
}

#[tokio::main]
async fn main() {
    let server = Server::bind(&SocketAddr::from(([127, 0, 0, 1], 8080))).serve(make_service_fn(
        |_conn| async { Ok::<_, Infallible>(service_fn(req_handler)) },
    ));

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
