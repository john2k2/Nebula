use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Fullscreen, Icon, Theme, WindowBuilder},
};
use wry::{
    NewWindowResponse, PermissionKind, PermissionResponse, WebContext, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
    http::{Request, Response, Uri},
};

const APP_URL: &str = "http://xuper.localhost/";
const COMMANDS: &[&str] = &[
    "cast.devices", "cast.start", "cast.status", "cast.pause", "cast.seek", "cast.stop", "cast.audio", "cast.volume",
    "update.check", "update.install", "update.progress",
    "status", "categories", "catalog", "search", "details", "playback", "favorites.list",
    "filters.options", "filters.catalog",
    "favorites.toggle", "playback.options", "playback.stop", "playback.subtitles", "playback.subtitle", "playback.report", "cast.open", "session.reconnect",
    "vlc.capabilities", "vlc.open", "vlc.status", "vlc.stop", "vlc.pause", "vlc.seek", "vlc.volume", "vlc.audio", "vlc.subtitle",
];

#[derive(Debug, Deserialize)]
struct Command {
    id: u64,
    command: String,
    #[serde(default = "empty_params")]
    params: Value,
}

#[derive(Debug)]
enum AppEvent {
    Quit,
    Response(Value),
    Smoke(Value),
    SmokeTimeout,
    NativeBuffer(u64),
    FrameAck(u64),
    Fullscreen(bool),
}

fn empty_params() -> Value {
    json!({})
}

fn failure(id: u64, code: &str, message: &str) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message}})
}

fn app_origin(uri: &Uri) -> bool {
    matches!(uri.scheme_str(), Some("http"))
        && uri.host() == Some("xuper.localhost")
        && uri.port_u16().is_none()
}

fn app_navigation(url: String) -> bool {
    url.parse::<Uri>()
        .map(|uri| app_origin(&uri) && matches!(uri.path(), "/" | "/index.html"))
        .unwrap_or(false)
}

fn asset(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let (status, mime, bytes): (u16, &str, &[u8]) = match (request.method().as_str(), request.uri().path()) {
        ("GET", "/" | "/index.html") => (200, "text/html; charset=utf-8", include_bytes!("../frontend/index.html")),
        ("GET", "/app.css") => (200, "text/css; charset=utf-8", include_bytes!("../frontend/app.css")),
        ("GET", "/app.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/app.js")),
        ("GET", "/release-ui.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/release-ui.js")),
        ("GET", "/cast-ui.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/cast-ui.js")),
        ("GET", "/player-controls.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/player-controls.js")),
        ("GET", "/player-controls.css") => (200, "text/css; charset=utf-8", include_bytes!("../frontend/player-controls.css")),
        ("GET", "/native-video.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/native-video.js")),
        ("GET", "/cinema.css") => (200, "text/css; charset=utf-8", include_bytes!("../frontend/cinema.css")),
        ("GET", "/vendor/hls-1.7.3.min.js") => (200, "text/javascript; charset=utf-8", include_bytes!("../frontend/vendor/hls-1.7.3.min.js")),
        ("GET", "/vendor/hls.LICENSE") => (200, "text/plain; charset=utf-8", include_bytes!("../frontend/vendor/hls.LICENSE")),
        ("GET", "/logo.png") => (200, "image/png", include_bytes!("../assets/nebula-logo.png")),
        _ => (404, "text/plain; charset=utf-8", b"Recurso no disponible"),
    };
    Response::builder()
        .status(status)
        .header("Content-Type", mime)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "no-store")
        .header("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' http: https: data:; media-src http: https: blob:; connect-src http: https:; worker-src 'self' blob:; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
        .body(Cow::Borrowed(bytes))
        .expect("constant asset headers are valid")
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    match std::env::var_os("XUPER_DATA_DIR") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(std::env::current_exe()?.parent().unwrap_or(std::path::Path::new(".")).join("data")),
    }
}

pub fn run<F>(handler: F, smoke: bool) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn(&str, &Value) -> Result<Value, String> + Send + Sync + 'static,
{
    let data_dir = data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let image_cache = crate::image_cache::ImageCache::new(data_dir.join("image-cache"))?;
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Nébula · Escritorio")
        .with_window_icon(Icon::from_rgba(include_bytes!("../assets/nebula-icon-64.rgba").to_vec(),64,64).ok())
        .with_inner_size(LogicalSize::new(1280.0, 820.0))
        .with_min_inner_size(LogicalSize::new(720.0, 560.0))
        .with_theme(Some(Theme::Dark))
        .with_visible(false)
        .build(&event_loop)?;

    // Network requests and local persistence never run on the window's event loop.
    let (sender, receiver) = mpsc::sync_channel::<Command>(32);
    let receiver = Arc::new(Mutex::new(receiver));
    let handler = Arc::new(handler);
    for index in 0..4 {
        let receiver = Arc::clone(&receiver);
        let handler = Arc::clone(&handler);
        let proxy = proxy.clone();
        let artwork = image_cache.clone();
        thread::Builder::new().name(format!("xuper-api-{index}")).spawn(move || loop {
            let command = match receiver.lock().map(|lock| lock.recv()) {
                Ok(Ok(command)) => command,
                _ => break,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handler(&command.command, &command.params)
            }));
            let response = match result {
                Ok(Ok(mut data)) => {
                    if matches!(command.command.as_str(), "catalog" | "search" | "details" | "favorites.list" | "filters.catalog") { artwork.rewrite(&mut data); }
                    json!({"id": command.id, "ok": true, "data": data})
                },
                Ok(Err(message)) => failure(command.id,
                    if crate::api::is_session_expired_error(&message) { "session_expired" } else { "service_error" }, &message),
                Err(_) => failure(command.id, "internal_error", "No se pudo completar la solicitud. Vuelve a intentarlo."),
            };
            if proxy.send_event(AppEvent::Response(response)).is_err() {
                break;
            }
        })?;
    }

    let mut context = WebContext::new(Some(data_dir.join(if smoke { "webview-smoke" } else { "webview" })));
    let ipc_proxy = proxy.clone();
    let stop_handler = Arc::clone(&handler);
    let artwork = image_cache.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color((20, 20, 20, 255))
        .with_theme(wry::Theme::Dark)
        .with_devtools(false)
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_autoplay(true)
        .with_asynchronous_custom_protocol("xuper".into(), move |_, request, responder| {
            if request.method().as_str() == "GET" && request.uri().path().starts_with("/images/") {
                artwork.serve(request.uri().path(), responder);
            } else { responder.respond(asset(request)); }
        })
        .with_navigation_handler(app_navigation)
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false)
        .with_permission_handler(|permission| {
            if matches!(permission, PermissionKind::Autoplay) {
                PermissionResponse::Allow
            } else {
                PermissionResponse::Deny
            }
        })
        .with_initialization_script(format!("Object.defineProperty(window, '__XUPER_SMOKE__', {{value:{smoke}}});"))
        .with_ipc_handler(move |request| {
            if !app_origin(request.uri()) || request.body().len() > 128 * 1024 {
                return;
            }
            let command = match serde_json::from_str::<Command>(request.body()) {
                Ok(command) => command,
                Err(_) => return,
            };
            if smoke && command.command == "app.smoke" {
                let _ = ipc_proxy.send_event(AppEvent::Smoke(command.params));
                return;
            }
            if command.command == "vlc.buffer" {
                let _ = ipc_proxy.send_event(AppEvent::NativeBuffer(command.id)); return;
            }
            if command.command == "vlc.frameAck" {
                if let Some(sequence) = command.params["sequence"].as_u64() { let _ = ipc_proxy.send_event(AppEvent::FrameAck(sequence)); }
                return;
            }
            if !COMMANDS.contains(&command.command.as_str()) || !command.params.is_object() {
                if command.command == "update.exit" && crate::updater::ready() {
                    let _ = ipc_proxy.send_event(AppEvent::Quit); return;
                }
                let _ = ipc_proxy.send_event(AppEvent::Response(failure(command.id, "invalid_request", "Solicitud no admitida.")));
                return;
            }
            if command.command == "playback.stop" {
                // API routes this to the relay manager before its session/network
                // mutex. Cancellation must not wait behind queued catalog work.
                let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    stop_handler(&command.command, &command.params)
                })) {
                    Ok(Ok(data)) => json!({"id": command.id, "ok": true, "data": data}),
                    Ok(Err(message)) => failure(command.id, "service_error", &message),
                    Err(_) => failure(command.id, "internal_error", "No se pudo cerrar la reproducción."),
                };
                let _ = ipc_proxy.send_event(AppEvent::Response(response));
                return;
            }
            if let Err(error) = sender.try_send(command) {
                let command = match error {
                    mpsc::TrySendError::Full(command) | mpsc::TrySendError::Disconnected(command) => command,
                };
                let _ = ipc_proxy.send_event(AppEvent::Response(failure(command.id, "busy", "Hay varias solicitudes en curso. Espera un momento y vuelve a intentarlo.")));
            }
        })
        .with_url(APP_URL)
        .build(&window)?;

    // HTML fullscreen must also expand the native window. WebView2 otherwise
    // keeps the video inside the client area, leaving the title bar visible.
    let fullscreen_proxy = proxy.clone();
    let mut fullscreen_token = 0;
    unsafe {
        webview.webview().add_ContainsFullScreenElementChanged(
            &webview2_com::ContainsFullScreenElementChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(sender) = sender {
                    let mut active = windows::core::BOOL(0);
                    sender.ContainsFullScreenElement(&mut active)?;
                    let _ = fullscreen_proxy.send_event(AppEvent::Fullscreen(active.as_bool()));
                }
                Ok(())
            })),
            &mut fullscreen_token,
        )?;
    }

    if smoke {
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(25));
            let _ = proxy.send_event(AppEvent::SmokeTimeout);
        });
    } else if std::env::var_os("XUPER_UI_TEST_HIDDEN").as_deref() != Some(std::ffi::OsStr::new("1")) {
        window.set_visible(true);
    }
    let mut native_frames = crate::native_player::Frames::new();
    event_loop.run(move |event, _, control_flow| {
        let _keep_context_alive = &context;
        *control_flow = ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(33));
        match event {
            Event::MainEventsCleared => native_frames.draw(&webview),
            Event::UserEvent(AppEvent::Fullscreen(active)) => {
                window.set_fullscreen(active.then(|| Fullscreen::Borderless(window.current_monitor())));
            }
            Event::UserEvent(AppEvent::FrameAck(sequence)) => native_frames.acknowledge(sequence),
            Event::UserEvent(AppEvent::NativeBuffer(id)) => {
                let response = match native_frames.prepare(&webview) {
                    Ok(data) => json!({"id":id,"ok":true,"data":data}),
                    Err(message) => failure(id,"vlc_unavailable",&message),
                };
                let encoded=serde_json::to_string(&response.to_string()).unwrap();
                let _=webview.evaluate_script(&format!("window.__xuperReceive(JSON.parse({encoded}));"));
            }
            Event::UserEvent(AppEvent::Response(response)) => {
                // Encode the payload as a JSON string; no service data becomes executable source.
                let json = response.to_string();
                if let Ok(encoded) = serde_json::to_string(&json) {
                    let _ = webview.evaluate_script(&format!("window.__xuperReceive(JSON.parse({encoded}));"));
                }
            }
            Event::UserEvent(AppEvent::Smoke(report)) => {
                let passed = report["navigation"] == 5
                    && report["hls"] == "1.7.3"
                    && report["bridge"] == true
                    && report["content"].as_str().is_some_and(|text| text.contains("No pudimos cargar"));
                let report = json!({"passed": passed, "webview": wry::webview_version().ok(), "ui": report});
                let _ = std::fs::write(data_dir.join("smoke-report.json"), serde_json::to_vec_pretty(&report).unwrap_or_default());
                println!("{report}");
                *control_flow = ControlFlow::ExitWithCode(if passed { 0 } else { 1 });
            }
            Event::UserEvent(AppEvent::SmokeTimeout) => {
                let report = json!({"passed": false, "error": "La interfaz no completó la comprobación de inicio."});
                let _ = std::fs::write(data_dir.join("smoke-report.json"), report.to_string());
                eprintln!("{report}");
                *control_flow = ControlFlow::ExitWithCode(1);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } | Event::UserEvent(AppEvent::Quit) => {
                image_cache.report();
                crate::native_player::shutdown();
                crate::native_cast::shutdown();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
