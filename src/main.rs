use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server};

use std::convert::Infallible;
use std::net::SocketAddr;

use serde_json;

mod get_otp;
mod monitor;

async fn req_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => {
            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "text/plain")
                .body(Body::from("Hello World!\r\n"))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-otp-qr") => {
            // TODO(adnanjpg): take from request
            let user_name = String::from("adnanjpg");

            let qr_code = get_otp::generate_otp_qr_code(user_name);

            match get_otp::outputqr(&qr_code) {
                Ok(qr) => {
                    // Print the QR code to the terminal
                    println!("{}\r\n{}", qr_code, qr);

                    let response = Response::builder()
                        .status(hyper::StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from("{\"success\": true}\r\n"))
                        .unwrap();
                    Ok(response)
                }
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                        .header("Content-Type", "text/plain")
                        .body(Body::from("Error generating QR code.\r\n"))
                        .unwrap();
                    Ok(response)
                }
            }
        }
        (&Method::POST, "/validate-totp") => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            // TODO: Generate and respond with jwt auth token

            // Parse the request body as JSON
            match serde_json::from_str::<get_otp::ValidateOtpData>(&body_str) {
                Ok(req_json) => match get_otp::check_totp_match(&req_json.token, get_otp::TOTP_KEY)
                {
                    true => {
                        let response = Response::builder()
                            .status(hyper::StatusCode::OK)
                            .header("Content-Type", "application/json")
                            .body(Body::from("{\"success\": true}\r\n"))
                            .unwrap();
                        Ok(response)
                    }
                    false => {
                        let response = Response::builder()
                            .status(hyper::StatusCode::UNAUTHORIZED)
                            .header("Content-Type", "text/plain")
                            .body(Body::from("Invalid token.\r\n"))
                            .unwrap();
                        Ok(response)
                    }
                },
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::BAD_REQUEST)
                        .header("Content-Type", "text/plain")
                        .body(Body::from("Invalid JSON.\r\n"))
                        .unwrap();
                    Ok(response)
                }
            }
        }
        (&Method::POST, "/register") => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            // TODO: Check request body has valid auth token

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from("{\"success\": true}\r\n"))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-desc") => {
            let desc = monitor::get_default_server_desc();

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&desc).unwrap() + "\r\n"))
                .unwrap();
            Ok(response)
        }
        (_, _) => {
            let response = Response::builder()
                .status(hyper::StatusCode::NOT_FOUND)
                .header("Content-Type", "text/plain")
                .body(Body::from("Not Found\r\n"))
                .unwrap();
            Ok(response)
        }
    }
}

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    let server = Server::bind(&addr).serve(make_service_fn(|_conn| async {
        Ok::<_, Infallible>(service_fn(req_handler))
    }));

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
