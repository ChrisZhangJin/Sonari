//! `sonari-server` — 引擎的进程外壳（WS + HTTP 静态资源，单端口）。
//!
//! 命令实现在 `sonari_engine::commands`（与 Tauri 桌面端共用），
//! 本 crate 只做传输：WS JSON 帧 ↔ 命令、HTTP 静态页面（内嵌 ui/dist）。
//!
//! 架构对标 ocenaudio 的多进程拆分（ocenaudio.exe GUI ↔ libocen 引擎）：
//! GUI/前端（Vue）通过 WebSocket JSON 命令总线驱动本进程内的引擎，
//! 引擎零 GUI 依赖（继承自 libocen 的 OCENCANVAS_* 抽象决策）。

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use futures_util::{SinkExt, StreamExt};
use sonari_engine::audio::{self, AudioState};
use sonari_engine::commands::{self, Docs};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

struct App {
    docs: Arc<RwLock<Docs>>,
    audio: Arc<AudioState>,
}

#[derive(Debug, Deserialize)]
struct Request {
    id: u64,
    cmd: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok(id: u64, data: Value) -> Self {
        Self { id, ok: true, data: Some(data), error: None }
    }
    fn err(id: u64, msg: impl Into<String>) -> Self {
        Self { id, ok: false, data: None, error: Some(msg.into()) }
    }
}

async fn handle_request(app: &Arc<App>, id: u64, cmd: &str, p: Value) -> Response {
    // CPU 密集（解码/FFT/编码）放阻塞线程池；RwLock 保证分析命令并发。
    // 命令分类由 `sonari_engine` 单点定义 —— 新增命令不必再改宿主。
    let docs = Arc::clone(&app.docs);
    let audio = Arc::clone(&app.audio);
    let cmd_owned = cmd.to_string();
    let res = tokio::task::spawn_blocking(move || {
        if sonari_engine::is_audio_cmd(&cmd_owned) {
            let docs = docs.read().unwrap();
            audio::dispatch(&audio, &docs, &cmd_owned, &p)
        } else if sonari_engine::is_write_cmd(&cmd_owned) {
            let mut docs = docs.write().unwrap();
            commands::dispatch_write(&mut docs, &cmd_owned, &p)
        } else {
            let docs = docs.read().unwrap();
            commands::dispatch_read(&docs, &cmd_owned, &p)
        }
    })
    .await;
    match res {
        Ok(Ok(data)) => Response::ok(id, data),
        Ok(Err(e)) => Response::err(id, e),
        Err(e) => Response::err(id, format!("join: {e}")),
    }
}

async fn handle_conn<S>(app: Arc<App>, stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("handshake: {e}");
            return;
        }
    };
    let (mut sink, mut src) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    // 写任务独占 sink；读循环产生的应答经 channel 回送
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    while let Some(msg) = src.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(_) => continue,
            Err(_) => break,
        };
        let req: Request = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Message::text(format!(
                    r#"{{"id":0,"ok":false,"error":"parse: {e}"}}"#
                )));
                continue;
            }
        };
        // 命令并发处理：慢命令（解码/FFT）不阻塞后续请求（nshell 教训）
        let app2 = app.clone();
        let id = req.id;
        let cmd = req.cmd.clone();
        let params = req.params;
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let resp = handle_request(&app2, id, &cmd, params).await;
            if let Ok(s) = serde_json::to_string(&resp) {
                let _ = tx2.send(Message::text(s));
            }
        });
    }
    drop(tx);
    let _ = writer.await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut addr = "127.0.0.1:9712".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--listen" {
            addr = args.next().unwrap_or(addr);
        }
    }
    let app = Arc::new(App {
        docs: Arc::new(RwLock::new(Docs::new())),
        audio: Arc::new(AudioState::default()),
    });
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let url = format!("http://{addr}");
    eprintln!("sonari-server listening on {url}  (UI + WebSocket 同端口)");
    if !std::env::var("NSHIP_NO_BROWSER").map(|v| v == "1").unwrap_or(false) {
        // 自动打开浏览器（失败无碍）
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("cmd")
                .args(["/c", "start", "", &url])
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        }
    }
    while let Ok((stream, _)) = listener.accept().await {
        let app = app.clone();
        tokio::spawn(dispatch(app, stream));
    }
    Ok(())
}

/// 请求分派：读 HTTP 头部，WS 升级请求转引擎命令总线，其余按静态文件应答。
async fn dispatch(app: Arc<App>, mut stream: TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16384 {
            return;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    if head.to_ascii_lowercase().contains("upgrade: websocket") {
        // 已消费的握手字节须重放给 tungstenite
        let prefixed = PrefixStream {
            prefix: std::io::Cursor::new(buf),
            inner: stream,
        };
        handle_conn(app, prefixed).await;
    } else {
        http::serve(&head, stream).await;
    }
}

/// 把「已预读的请求头」拼回流前面 —— WS 握手需要完整字节序列。
struct PrefixStream {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl tokio::io::AsyncRead for PrefixStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if (this.prefix.position() as usize) < this.prefix.get_ref().len() {
            Pin::new(&mut this.prefix).poll_read(cx, buf)
        } else {
            Pin::new(&mut this.inner).poll_read(cx, buf)
        }
    }
}

impl tokio::io::AsyncWrite for PrefixStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 内嵌 UI 静态资源（编译期打包 ui/dist，构建前先 `npm run build`）。
mod http {
    use include_dir::{include_dir, Dir};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/../ui/dist");

    fn content_type(path: &str) -> &'static str {
        match path.rsplit('.').next().unwrap_or("") {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "json" => "application/json",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "wasm" => "application/wasm",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        }
    }

    pub async fn serve(head: &str, mut stream: TcpStream) {
        // 请求行：GET /path HTTP/1.1
        let path = head
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .split('?')
            .next()
            .unwrap_or("/")
            .to_string();
        let rel = path.trim_start_matches('/');
        let rel = if rel.is_empty() { "index.html" } else { rel };

        let (status, body): (&str, Option<&[u8]>) = match UI_DIST.get_file(rel) {
            Some(f) => ("200 OK", Some(f.contents())),
            // SPA 兜底：未命中路径回 index.html
            None => match UI_DIST.get_file("index.html") {
                Some(f) => ("200 OK", Some(f.contents())),
                None => ("404 Not Found", Some(b"ui/dist missing: run `npm run build` in ui/" as &[u8])),
            },
        };
        let ct = content_type(if body.is_some() && status.starts_with("200") { rel } else { "txt" });
        let hdr = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.map(|b| b.len()).unwrap_or(0)
        );
        let _ = stream.write_all(hdr.as_bytes()).await;
        if let Some(b) = body {
            let _ = stream.write_all(b).await;
        }
        let _ = stream.shutdown().await;
    }
}
