use wstd::http::{Body, Request, Response};

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let path = req.uri().path();
    let id = path.strip_prefix("/user/").unwrap_or_default();

    Ok(Response::new(format!("USER: {}\n", id).into()))
}
