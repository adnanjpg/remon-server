use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};

use std::convert::Infallible;
use std::net::SocketAddr;

mod get_otp_qr;
mod get_totp;

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
            let qr_code = get_otp_qr::generate_otp_qr_code();

            get_otp_qr::outputqr(&qr_code.to_string()).unwrap();

            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(""))
                .unwrap();

            Ok(response)
        }
        "/get-current-totp" => {
            // The TOTP time step is typically 30 seconds.
            let time_step = 30;

            // TODO(adnanjpg): get from saved secret
            let randomk = "F23FZGOU3CW4AYDQYXAGUNUYKLCHKVYB";

            let totp = get_totp::generate_totp(randomk.to_owned(), time_step);

            println!("totp iss {}", totp);

            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(""))
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
