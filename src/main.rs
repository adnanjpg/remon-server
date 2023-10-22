use hyper::{Body, Request, Response, Server};
use hyper::service::{make_service_fn, service_fn};

use std::convert::Infallible;
use std::net::SocketAddr;

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

    let server = Server::bind(&SocketAddr::from(([127,0,0,1],8080))).serve(
        make_service_fn(|_conn| {
            async {
                Ok::<_, Infallible>(service_fn(req_handler))
            }
        }
    ));

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
