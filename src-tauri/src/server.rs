use crate::hub::{classify, mime_for, resolve_file, FileItem, Hub, NetInfo, Shared};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

const PHONE_HTML: &str = include_str!("phone.html");

const GATE_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="robots" content="noindex">
  <link rel="icon" href="/favicon.svg">
  <title>快传</title>
  <style>
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #efece4; color: #1c1915; font-family: "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif; }
    .card { width: min(440px, calc(100% - 32px)); background: #fffdf8; border: 1px solid #e3dcd0; border-radius: 18px; padding: 28px; }
    h1 { margin: 0 0 8px; font-size: 28px; }
    p { margin: 0; color: #6f675e; line-height: 1.7; }
  </style>
</head>
<body>
  <div class="card">
    <h1>快传</h1>
    <p>请用电脑窗口上的二维码打开。手机和电脑要连同一个 Wi-Fi。</p>
  </div>
</body>
</html>"#;

const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#0f6e56"/><path d="M8 16h12M16 11l5 5-5 5" fill="none" stroke="#fff" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"##;

#[derive(Deserialize, Default)]
struct TokenQuery {
    #[serde(default)]
    k: Option<String>,
    #[serde(default)]
    inline: Option<u8>,
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

pub fn router(hub: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/files", get(api_files))
        .route("/api/upload", post(upload))
        .route("/api/delete", post(api_delete))
        .route("/api/clear", post(api_clear))
        .route("/dl/{name}", get(download))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .fallback(not_found)
        .layer(DefaultBodyLimit::disable())
        .with_state(hub)
}

pub async fn serve(hub: Shared) -> Result<(), String> {
    let listener = match bind_listener(8787).await {
        Ok(listener) => listener,
        Err(err) => {
            hub.set_boot_error(err.clone());
            return Err(err);
        }
    };
    let port = listener
        .local_addr()
        .map_err(|err| err.to_string())?
        .port();
    let ips = lan_ipv4s();
    let (primary, urls, no_lan) = urls_for(&ips, port, &hub.token);
    let qr_svg = make_qr(&primary).unwrap_or_default();
    println!();
    println!("  快传已启动");
    println!("  手机打开  {primary}");
    println!("  文件保存在 {}", hub.share_dir.display());
    if no_lan {
        println!("  没有发现局域网地址，手机可能连不上");
    }
    println!("  窗口关掉后，手机就进不来了");
    println!();
    hub.publish_net(NetInfo {
        port,
        urls,
        primary_url: primary,
        qr_svg,
        no_lan,
    });
    let app = router(hub);
    axum::serve(listener, app)
        .await
        .map_err(|err| err.to_string())
}

async fn bind_listener(start: u16) -> Result<tokio::net::TcpListener, String> {
    let mut last = String::from("未知错误");
    for port in start..start.saturating_add(20) {
        match tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(listener) => return Ok(listener),
            Err(err) => last = err.to_string(),
        }
    }
    Err(format!("端口 {start} 开始的 20 个都被占用了：{last}"))
}

fn lan_ipv4s() -> Vec<String> {
    let mut ips = Vec::new();
    let mut push = |ip: Ipv4Addr| {
        if ip.is_loopback() || ip.is_unspecified() {
            return;
        }
        let text = ip.to_string();
        if !ips.contains(&text) {
            ips.push(text);
        }
    };
    if let Ok(ip) = local_ip_address::local_ip() {
        if let std::net::IpAddr::V4(ip) = ip {
            push(ip);
        }
    }
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in list {
            if let std::net::IpAddr::V4(ip) = ip {
                push(ip);
            }
        }
    }
    ips
}

fn rank(ip: &str) -> u8 {
    if ip.starts_with("169.254.") || is_benchmark(ip) {
        2
    } else if ip.starts_with("192.168.") || ip.starts_with("10.") || is_172(ip) {
        0
    } else {
        1
    }
}

fn is_benchmark(ip: &str) -> bool {
    let mut parts = ip.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return false;
    };
    first == "198" && matches!(second, "18" | "19")
}

fn is_172(ip: &str) -> bool {
    let mut parts = ip.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return false;
    };
    first == "172"
        && second
            .parse::<u8>()
            .map(|value| (16..=31).contains(&value))
            .unwrap_or(false)
}

fn urls_for(ips: &[String], port: u16, token: &str) -> (String, Vec<String>, bool) {
    let mut ordered = ips.to_vec();
    ordered.sort_by_key(|ip| rank(ip));
    let urls: Vec<String> = ordered
        .iter()
        .filter(|ip| rank(ip) < 2)
        .map(|ip| format!("http://{ip}:{port}/?k={token}"))
        .collect();
    if let Some(primary) = urls.first() {
        (primary.clone(), urls, false)
    } else {
        let local = format!("http://127.0.0.1:{port}/?k={token}");
        (local.clone(), vec![local], true)
    }
}

pub fn make_qr(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|err| err.to_string())?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .quiet_zone(true)
        .min_dimensions(220, 220)
        .build();
    let start = svg
        .find("<svg")
        .ok_or_else(|| "二维码生成失败".to_string())?;
    Ok(svg[start..].to_string())
}

async fn index(
    State(hub): State<Shared>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&hub, query.k.as_deref(), &headers) {
        return html_response(StatusCode::OK, GATE_HTML.to_string(), None);
    }
    let boot = serde_json::json!({
        "token": hub.token,
        "serverName": hub.server_name,
        "files": hub.snapshot(),
    });
    let payload = boot.to_string().replace('<', "\\u003c");
    let html = PHONE_HTML.replace("__BOOT_JSON__", &payload);
    let cookie = format!("k={}; Path=/; SameSite=Lax; HttpOnly", hub.token);
    html_response(StatusCode::OK, html, Some(cookie))
}

async fn api_files(
    State(hub): State<Shared>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = deny(&hub, query.k.as_deref(), &headers) {
        return response;
    }
    json_ok(serde_json::json!({ "ok": true, "files": hub.snapshot() }))
}

async fn upload(
    State(hub): State<Shared>,
    Query(query): Query<TokenQuery>,
    request: axum::extract::Request,
) -> Response {
    if let Some(response) = deny(&hub, query.k.as_deref(), request.headers()) {
        return response;
    }
    let multipart = match Multipart::from_request(request, &()).await {
        Ok(multipart) => multipart,
        Err(_) => return json_msg(StatusCode::BAD_REQUEST, "上传内容不对"),
    };
    match receive_upload(&hub, multipart).await {
        Ok(files) => json_ok(serde_json::json!({ "ok": true, "files": files })),
        Err(message) => json_msg(StatusCode::BAD_REQUEST, &message),
    }
}

async fn discard_staged(hub: &Hub, staged: impl IntoIterator<Item = (PathBuf, String)>) {
    for (tmp, name) in staged {
        let _ = tokio::fs::remove_file(&tmp).await;
        hub.unreserve(&name);
    }
}

async fn receive_upload(hub: &Hub, mut multipart: Multipart) -> Result<Vec<FileItem>, String> {
    let mut device = String::new();
    let mut client_id = String::new();
    let mut staged: Vec<(PathBuf, String)> = Vec::new();

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                eprintln!("读取上传失败: {err}");
                discard_staged(hub, staged).await;
                return Err("上传中断了，请重试".into());
            }
        };
        let field_name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|name| name.to_string());
        if field_name == "device" {
            device = clean_label(&read_limited(field, 1024).await, 40);
            continue;
        }
        if field_name == "clientId" {
            client_id = clean_id(&read_limited(field, 128).await);
            continue;
        }
        if file_name.is_none() && field_name != "file" {
            let _ = read_limited(field, 1024).await;
            continue;
        }
        let original = file_name.unwrap_or_else(|| "file".into());
        let name = hub.reserve_name(&original);
        let tmp = hub.share_dir.join(format!(".part-{}", uuid::Uuid::new_v4()));
        let mut file = match tokio::fs::File::create(&tmp).await {
            Ok(file) => file,
            Err(err) => {
                hub.unreserve(&name);
                discard_staged(hub, staged).await;
                eprintln!("创建临时文件失败: {err}");
                return Err(io_text(&err));
            }
        };
        let mut failed: Option<std::io::Error> = None;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if let Err(err) = file.write_all(&chunk).await {
                        failed = Some(err);
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    eprintln!("读取上传数据失败: {err}");
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp).await;
                    hub.unreserve(&name);
                    discard_staged(hub, staged).await;
                    return Err("上传中断了，请重试".into());
                }
            }
        }
        if let Err(err) = file.flush().await {
            failed.get_or_insert(err);
        }
        drop(file);
        if let Some(err) = failed {
            let _ = tokio::fs::remove_file(&tmp).await;
            hub.unreserve(&name);
            discard_staged(hub, staged).await;
            eprintln!("写入失败: {err}");
            return Err(io_text(&err));
        }
        staged.push((tmp, name));
    }

    if staged.is_empty() {
        return Err("没有收到文件".into());
    }

    let mut saved = Vec::new();
    let mut pending = staged.into_iter();
    while let Some((tmp, name)) = pending.next() {
        let dest = hub.share_dir.join(&name);
        if let Err(err) = tokio::fs::rename(&tmp, &dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            hub.unreserve(&name);
            discard_staged(hub, pending).await;
            eprintln!("保存失败: {err}");
            return Err(io_text(&err));
        }
        let item = hub.remember_upload(&name, &device, &client_id);
        let from = if device.is_empty() {
            String::new()
        } else {
            format!(" 来自 {device}")
        };
        eprintln!("收到 {} ({}){from}", item.name, human_size(item.size));
        saved.push(item);
    }
    Ok(saved)
}

async fn read_limited(mut field: axum::extract::multipart::Field<'_>, max: usize) -> String {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = field.chunk().await {
        if buf.len() >= max {
            break;
        }
        let room = max - buf.len();
        let take = room.min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

async fn api_delete(
    State(hub): State<Shared>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NameBody>,
) -> Response {
    if let Some(response) = deny(&hub, query.k.as_deref(), &headers) {
        return response;
    }
    match hub.delete(&body.name) {
        Ok(()) => json_ok(serde_json::json!({ "ok": true })),
        Err(message) => json_msg(StatusCode::BAD_REQUEST, &message),
    }
}

async fn api_clear(
    State(hub): State<Shared>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = deny(&hub, query.k.as_deref(), &headers) {
        return response;
    }
    match hub.clear() {
        Ok(removed) => json_ok(serde_json::json!({ "ok": true, "removed": removed })),
        Err(message) => json_msg(StatusCode::BAD_REQUEST, &message),
    }
}

async fn download(
    State(hub): State<Shared>,
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = deny(&hub, query.k.as_deref(), &headers) {
        return response;
    }
    let Some(path) = resolve_file(&hub.share_dir, &name) else {
        return json_msg(StatusCode::NOT_FOUND, "没有这个文件");
    };
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or(name);
    let (_kind, preview) = classify(&file_name);
    let inline = query.inline == Some(1) && preview;
    let mime = if inline {
        mime_for(&file_name, true)
    } else {
        "application/octet-stream"
    };
    let size = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.len(),
        Err(err) => {
            eprintln!("读取文件信息失败: {err}");
            return json_msg(StatusCode::NOT_FOUND, "没有这个文件");
        }
    };
    let (start, end, status) = match range_bounds(headers.get(header::RANGE), size) {
        Ok(bounds) => bounds,
        Err(response) => return response,
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(inline, &file_name),
        )
        .header("x-content-type-options", "nosniff")
        .header(header::CACHE_CONTROL, "no-store");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{size}"),
        );
    }
    if length == 0 {
        return builder.body(Body::empty()).unwrap();
    }
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => {
            eprintln!("打开文件失败: {err}");
            return json_msg(StatusCode::NOT_FOUND, "没有这个文件");
        }
    };
    if start > 0 {
        if let Err(err) = file.seek(std::io::SeekFrom::Start(start)).await {
            eprintln!("定位文件失败: {err}");
            return json_msg(StatusCode::INTERNAL_SERVER_ERROR, "读取失败");
        }
    }
    let stream = ReaderStream::new(file.take(length));
    builder.body(Body::from_stream(stream)).unwrap()
}

fn range_bounds(
    header_value: Option<&axum::http::HeaderValue>,
    size: u64,
) -> Result<(u64, u64, StatusCode), Response> {
    if size == 0 {
        return Ok((0, 0, StatusCode::OK));
    }
    let Some(value) = header_value.and_then(|value| value.to_str().ok()) else {
        return Ok((0, size - 1, StatusCode::OK));
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Ok((0, size - 1, StatusCode::OK));
    };
    let spec = spec.split(',').next().unwrap_or("");
    let Some((start_text, end_text)) = spec.split_once('-') else {
        return Ok((0, size - 1, StatusCode::OK));
    };
    if start_text.is_empty() {
        let Ok(tail) = end_text.parse::<u64>() else {
            return Ok((0, size - 1, StatusCode::OK));
        };
        if tail == 0 {
            return Err(range_error(size));
        }
        let start = size.saturating_sub(tail);
        return Ok((start, size - 1, StatusCode::PARTIAL_CONTENT));
    }
    let Ok(start) = start_text.parse::<u64>() else {
        return Ok((0, size - 1, StatusCode::OK));
    };
    let end = if end_text.is_empty() {
        size - 1
    } else {
        end_text.parse::<u64>().unwrap_or(size - 1).min(size - 1)
    };
    if start >= size || start > end {
        return Err(range_error(size));
    }
    Ok((start, end, StatusCode::PARTIAL_CONTENT))
}

fn range_error(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Body::empty())
        .unwrap()
}

async fn favicon() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(FAVICON))
        .unwrap()
}

async fn not_found() -> Response {
    json_msg(StatusCode::NOT_FOUND, "没有这个页面")
}

fn authorized(hub: &Hub, query_token: Option<&str>, headers: &HeaderMap) -> bool {
    if query_token == Some(hub.token.as_str()) {
        return true;
    }
    cookie_token(headers).as_deref() == Some(hub.token.as_str())
}

fn deny(hub: &Hub, query_token: Option<&str>, headers: &HeaderMap) -> Option<Response> {
    if authorized(hub, query_token, headers) {
        None
    } else {
        Some(json_msg(
            StatusCode::UNAUTHORIZED,
            "口令不对，请重新扫二维码",
        ))
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("k=") {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn content_disposition(inline: bool, filename: &str) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let ascii: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii() && ch != '"' && ch != '\\' && !ch.is_control() {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let (stem, ext) = match ascii.rfind('.') {
        Some(index) if index > 0 && ascii.len() - index <= 12 => (&ascii[..index], &ascii[index..]),
        _ => (ascii.as_str(), ""),
    };
    let stem_ok = stem.chars().any(|ch| ch.is_ascii_alphanumeric());
    let ext = ext.to_string();
    let ascii = if stem_ok {
        ascii
    } else if ext.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        format!("download{ext}")
    } else {
        "download.bin".to_string()
    };
    format!(
        "{kind}; filename=\"{ascii}\"; filename*=UTF-8''{}",
        percent_encode(filename)
    )
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn clean_label(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

fn clean_id(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(40)
        .collect()
}

fn io_text(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(112) | Some(28) => "磁盘空间不足".into(),
        Some(32) | Some(33) => "文件正在被占用".into(),
        _ => "保存文件失败".into(),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn json_ok(value: serde_json::Value) -> Response {
    json_body(StatusCode::OK, value)
}

fn json_msg(status: StatusCode, error: &str) -> Response {
    json_body(status, serde_json::json!({ "ok": false, "error": error }))
}

fn json_body(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn html_response(status: StatusCode, html: String, cookie: Option<String>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff");
    if let Some(cookie) = cookie {
        builder = builder.header(header::SET_COOKIE, cookie);
    }
    builder.body(Body::from(html)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct TempTree(PathBuf);
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_hub() -> (Shared, TempTree) {
        let root = std::env::temp_dir().join(format!("kuaichuan-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("share")).unwrap();
        let hub = Shared::new(Hub::new(
            root.join("share"),
            root.join("meta.json"),
            "TEST-PC".into(),
        ));
        (hub, TempTree(root))
    }

    async fn bytes_of(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    fn multipart(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"device\"\r\n\r\n手机\r\n").as_bytes(),
        );
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"clientId\"\r\n\r\nphone123\r\n").as_bytes(),
        );
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend(content);
        body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    #[test]
    fn qr_is_svg() {
        let svg = make_qr("http://192.168.1.8:8787/?k=abc123").unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("</svg>"));
        assert!(!svg.contains("<?xml"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_token_is_rejected() {
        let (hub, _tree) = test_hub();
        let token = hub.token.clone();
        let app = router(hub);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/files")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let page = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = String::from_utf8(bytes_of(page).await).unwrap();
        assert!(!html.contains(&token));
        assert!(html.contains("二维码"));
        let rejected = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/upload")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_download_and_delete() {
        let (hub, tree) = test_hub();
        let token = hub.token.clone();
        let app = router(hub.clone());
        let payload = b"hello \x00\xff\r\n--nope \xe4\xb8\xad";
        let boundary = "----KuaiBoundary7k";
        let body = multipart(boundary, "../../\u{7167}\u{7247}.txt", payload);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/upload?k={token}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let saved: serde_json::Value =
            serde_json::from_slice(&bytes_of(response).await).unwrap();
        let name = saved["files"][0]["name"].as_str().unwrap().to_string();
        assert_eq!(name, "照片.txt");
        assert_eq!(saved["files"][0]["from"], "手机");
        assert_eq!(saved["files"][0]["clientId"], "phone123");
        assert!(!tree.0.join("照片.txt").exists());
        let stored = std::fs::read(tree.0.join("share").join(&name)).unwrap();
        assert_eq!(stored, payload);

        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/files?k={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&bytes_of(listed).await).unwrap();
        assert_eq!(listed["files"][0]["name"], name);

        let downloaded = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/dl/{}?k={token}", super::percent_encode(&name)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(bytes_of(downloaded).await, payload);

        let outside = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/dl/..%2F..%2Fsecret.txt?k={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outside.status(), StatusCode::NOT_FOUND);

        let removed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/delete?k={token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::OK);
        assert!(hub.snapshot().is_empty());
    }

    #[test]
    fn extension_and_rank() {
        assert_eq!(crate::hub::extension("照片.JPG"), "jpg");
        assert_eq!(super::rank("192.168.1.8"), 0);
        assert_eq!(super::rank("172.17.94.79"), 0);
        assert_eq!(super::rank("8.8.8.8"), 1);
        assert_eq!(super::rank("198.18.0.1"), 2);
        assert_eq!(super::rank("169.254.1.1"), 2);
        assert!(super::is_172("172.16.0.1"));
        assert!(!super::is_172("172.32.0.1"));
    }
}
