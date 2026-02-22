use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

static WEB_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web");

pub async fn serve_embedded_ui(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    let path = if raw_path.is_empty() { "index.html" } else { raw_path };

    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Some(file) = WEB_DIR.get_file(path) {
        return file_response(path, file.contents());
    }

    if let Some(index) = WEB_DIR.get_file("index.html") {
        return file_response("index.html", index.contents());
    }

    StatusCode::NOT_FOUND.into_response()
}

fn file_response(path: &str, content: &[u8]) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = content.to_vec().into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    response
}
