use axum::http::StatusCode;

pub async fn hello() -> &'static str {
    "Hello World!"
}

pub async fn teapot() -> (StatusCode, &'static str) {
    (StatusCode::IM_A_TEAPOT, "I'm a teapot!")
}

pub async fn healthcheck() -> &'static str {
    "Running smoothly!"
}
