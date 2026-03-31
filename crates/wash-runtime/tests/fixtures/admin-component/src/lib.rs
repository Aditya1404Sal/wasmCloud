use wstd::http::{Body, Request, Response};

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    Ok(Response::new("ADMIN\n".into()))
}
