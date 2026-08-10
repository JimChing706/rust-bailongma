//! 静态资源服务（对齐 Node 版 `src/api/routes/static.js`）。
//!
//! 作为 axum fallback 挂载：API 路由未匹配时按路径表服务前端页面与资产。
//! - HTML 页面：`/`、`/brain-ui`、`/dashboard.html`、`/activation` 等
//! - 资产根：`/src/ui/brain-ui/*`、`/src/ui/scene-shell/*`（路径穿越防护）
//! - 资源根目录解析（对齐 paths.js RESOURCES_DIR）：
//!   环境变量 `BAILONGMA_RESOURCES_DIR` → 缺省 workspace `resources/`
//! - UI 资产未平移时页面缺失返回 404 / JSON 欢迎页，不影响 API 功能

use std::fs;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};

/// 解析资源根目录（对齐 paths.js：BAILONGMA_RESOURCES_DIR 环境变量优先）。
pub fn resolve_resources_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BAILONGMA_RESOURCES_DIR") {
        let p = PathBuf::from(dir.trim());
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    // 开发模式：workspace 根下的 resources/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("resources"))
        .unwrap_or_else(|| PathBuf::from("resources"))
}

/// 静态路由处理器（对齐 handleStaticRoutes；返回 None 表示未匹配）。
/// 调用方（fallback）负责把 None 转成 404。
pub fn handle_static(
    req: &Request<Body>,
    resources: &Path,
    needs_activation: bool,
) -> Option<Response<Body>> {
    if req.method() != axum::http::Method::GET {
        return None;
    }
    let pathname = req.uri().path();

    match pathname {
        "/turn-trace" | "/turn-trace.html" => {
            return Some(serve_html(
                resources,
                "turn-trace.html",
                "turn-trace.html not found",
            ));
        }
        "/favicon.ico" => {
            return Some(
                Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Body::empty())
                    .unwrap(),
            );
        }
        "/activation" | "/activation.html" => {
            return Some(serve_html(
                resources,
                "activation.html",
                "activation.html not found",
            ));
        }
        "/" | "/index.html" => {
            if needs_activation {
                return Some(redirect("/activation"));
            }
            return Some(serve_index_or_welcome(resources));
        }
        "/dashboard.html" => {
            return Some(serve_html(
                resources,
                "dashboard.html",
                "dashboard.html not found",
            ));
        }
        "/brain.html" => {
            return Some(serve_html(resources, "brain.html", "brain.html not found"));
        }
        "/site" | "/site.html" => {
            return Some(serve_html(resources, "site.html", "website.html not found"));
        }
        "/site-assets/icon.png" => {
            return Some(serve_file(
                &resources.join("build").join("icon.png"),
                "site icon not found",
                "public, max-age=31536000, immutable",
            ));
        }
        "/brain-ui" | "/brain-ui.html" => {
            if needs_activation {
                return Some(redirect("/activation"));
            }
            return Some(serve_html(
                resources,
                "brain-ui.html",
                "brain-ui.html not found",
            ));
        }
        "/terminal-stream" | "/terminal-stream.html" => {
            return Some(serve_html(
                resources,
                "terminal-stream.html",
                "terminal-stream.html not found",
            ));
        }
        "/systemPrompt.html" => {
            return Some(serve_html(
                resources,
                "systemPrompt.html",
                "systemPrompt.html not found",
            ));
        }
        "/vendor/d3/d3.min.js" => {
            return Some(serve_file(
                &resources
                    .join("node_modules")
                    .join("d3")
                    .join("dist")
                    .join("d3.min.js"),
                "d3.min.js not found",
                "public, max-age=31536000, immutable",
            ));
        }
        _ => {}
    }

    // 资产根（对齐 serveAsset）：路径穿越防护 + 类型
    if pathname.starts_with("/src/ui/scene-shell/") {
        return Some(serve_asset(
            resources,
            &resources.join("src").join("ui").join("scene-shell"),
            "/src/ui/scene-shell/",
            req,
        ));
    }
    if pathname.starts_with("/src/ui/brain-ui/") {
        return Some(serve_asset(
            resources,
            &resources.join("src").join("ui").join("brain-ui"),
            "/src/ui/brain-ui/",
            req,
        ));
    }

    None
}

/// `/`：优先 index.html；资产未平移时回退 JSON 欢迎页（服务可用性自证）。
fn serve_index_or_welcome(resources: &Path) -> Response<Body> {
    let index = resources.join("index.html");
    if index.is_file() {
        return serve_html(resources, "index.html", "index.html not found");
    }
    let body = serde_json::json!({
        "ok": true,
        "service": "bailongma",
        "endpoints": ["/message", "/events", "/events/history", "/status", "/scene"],
        "note": "UI assets not deployed; set BAILONGMA_RESOURCES_DIR to serve brain-ui."
    })
    .to_string();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

fn redirect(location: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .unwrap()
}

fn serve_html(resources: &Path, name: &str, not_found: &str) -> Response<Body> {
    let path = resources.join(name);
    serve_file(&path, not_found, "no-cache")
}

fn serve_file(path: &Path, not_found: &str, cache_control: &str) -> Response<Body> {
    match fs::read(path) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type_for(path))
            .header(header::CONTENT_LENGTH, bytes.len())
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => plain_text(StatusCode::NOT_FOUND, not_found),
    }
}

fn serve_asset(resources: &Path, root: &Path, prefix: &str, req: &Request<Body>) -> Response<Body> {
    // 相对路径：URL 去掉前缀（对齐 serveAsset 的 slice）
    let raw = &req.uri().path()[prefix.len()..];
    let relative = match urlencoding_decode(raw) {
        Some(rel) => rel,
        None => return plain_text(StatusCode::BAD_REQUEST, "bad request"),
    };
    if relative.is_empty() || relative.starts_with('/') {
        return plain_text(StatusCode::NOT_FOUND, "asset not found");
    }
    let asset_path = root.join(&relative);
    // 路径穿越防护（对齐 isPathInside；canonicalize 统一真实路径，杜绝 `..` / 大小写绕过）
    if !is_path_inside(resources, root, &asset_path) {
        return plain_text(StatusCode::FORBIDDEN, "forbidden");
    }
    if !asset_path.is_file() {
        return plain_text(StatusCode::NOT_FOUND, "asset not found");
    }
    match fs::read(&asset_path) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type_for(&asset_path))
            .header(header::CONTENT_LENGTH, bytes.len())
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => plain_text(StatusCode::NOT_FOUND, "asset not found"),
    }
}

/// 路径穿越防护：canonicalize 后要求 asset 在 root 之下。
/// 传入 resources 仅用于在 root 不存在时快速拒绝。
fn is_path_inside(_resources: &Path, root: &Path, candidate: &Path) -> bool {
    let Ok(root_c) = fs::canonicalize(root) else {
        return false;
    };
    let Ok(cand_c) = fs::canonicalize(candidate) else {
        return false;
    };
    cand_c.starts_with(&root_c)
}

/// 简单百分号解码（仅静态资产路径用，UTF-8）。
fn urlencoding_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn plain_text(status: StatusCode, text: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(text.to_string()))
        .unwrap()
}

/// Content-Type 表（对齐 utils.js contentTypeFor 常用子集）。
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src/ui/brain-ui")).unwrap();
        fs::create_dir_all(root.join("src/ui/scene-shell")).unwrap();
        fs::write(root.join("index.html"), "<html>index</html>").unwrap();
        fs::write(root.join("src/ui/brain-ui/app.js"), "console.log(1)").unwrap();
        fs::write(root.join("src/ui/scene-shell/shell.js"), "shell()").unwrap();
        (dir, root)
    }

    fn get(pathname: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(pathname)
            .body(Body::empty())
            .unwrap()
    }

    async fn body_text(resp: Response<Body>) -> String {
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn serves_index_html_or_welcome() {
        let (_d, root) = fixture();
        let resp = handle_static(&get("/"), &root, false).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_text(resp).await, "<html>index</html>");

        // 资源缺失时回退 JSON 欢迎
        let empty = tempfile::tempdir().unwrap();
        let resp = handle_static(&get("/"), empty.path(), false).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_text(resp).await.contains("\"ok\":true"));
    }

    #[test]
    fn activation_redirects_when_needed() {
        let (_d, root) = fixture();
        let resp = handle_static(&get("/"), &root, true).unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(resp.headers()[header::LOCATION], "/activation");
        let resp = handle_static(&get("/brain-ui"), &root, true).unwrap();
        assert_eq!(resp.headers()[header::LOCATION], "/activation");
    }

    #[tokio::test]
    async fn serves_assets_with_type_and_cache() {
        let (_d, root) = fixture();
        let resp = handle_static(&get("/src/ui/brain-ui/app.js"), &root, false).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript"));
        let resp = handle_static(&get("/src/ui/scene-shell/shell.js"), &root, false).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_text(resp).await, "shell()");
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (_d, root) = fixture();
        let resp = handle_static(&get("/src/ui/brain-ui/../../index.html"), &root, false).unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp =
            handle_static(&get("/src/ui/brain-ui/..%2f..%2findex.html"), &root, false).unwrap();
        assert!(resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unknown_path_is_none() {
        let (_d, root) = fixture();
        assert!(handle_static(&get("/no/such/path"), &root, false).is_none());
        assert!(handle_static(&get("/src/other"), &root, false).is_none());
    }

    #[test]
    fn favicon_is_204() {
        let (_d, root) = fixture();
        let resp = handle_static(&get("/favicon.ico"), &root, false).unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn percent_decoding_handles_utf8() {
        assert_eq!(urlencoding_decode("a%20b%2Fc").as_deref(), Some("a b/c"));
        assert_eq!(urlencoding_decode("%E4%BD%A0"), Some("你".into()));
        assert_eq!(urlencoding_decode("bad%zz"), None);
        assert_eq!(urlencoding_decode("a+b"), Some("a b".into()));
    }
}
