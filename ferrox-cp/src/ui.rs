/// Serves the embedded React SPA.
///
/// Files under `ui/dist/` are embedded into the binary at compile time via
/// `include_dir!`.  Axum uses this as a fallback handler: API routes take
/// priority; any other path either returns the matching static file or falls
/// back to `index.html` so the React router can handle client-side navigation.
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::Response;
use include_dir::{include_dir, Dir};

static UI_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

pub async fn serve_spa(req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    if let Some(file) = UI_DIR.get_file(path) {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from(file.contents()))
            .unwrap_or_else(|_| internal_error());
    }

    // SPA fallback: serve index.html for unknown paths so the React router works.
    if let Some(index) = UI_DIR.get_file("index.html") {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(index.contents()))
            .unwrap_or_else(|_| internal_error());
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap_or_else(|_| internal_error())
}

fn internal_error() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .unwrap()
}
