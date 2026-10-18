//! Backend-authorized media relays. Desktop playback stays on loopback; Cast
//! explicitly exports a verified session to one concrete private IPv4 address.
//! Neither listener exposes an HTTP endpoint accepting destination URLs.
//! LL-HLS reload directives (_HLS_msn, _HLS_part, _HLS_skip) are not forwarded:
//! resource routes currently accept only the opaque path, without a query.
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

use reqwest::{
    Url,
    blocking::{Client, ClientBuilder, Response as UpstreamResponse},
    header::{HeaderMap, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RANGE},
    redirect::Policy,
};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const UI_ORIGIN: &str = "http://xuper.localhost";
const MAX_PLAYLIST: usize = 1024 * 1024;
const MAX_REWRITTEN_PLAYLIST: usize = 4 * 1024 * 1024;
const MAX_RESOURCES: usize = 8192;
const WORKERS: usize = 6;
const MAX_SESSIONS: usize = 4;
const MAX_CLIENTS: usize = 32;
const SESSION_IDLE: Duration = Duration::from_secs(20 * 60);
const PROBE_BYTES: usize = 4096;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
const UNKNOWN_MEDIA: &str = "La fuente no devolvió un video MP4, MPEG-TS ni una lista HLS compatible.";
const VERIFY_TIMEOUT_MESSAGE: &str = "La fuente tardó demasiado en responder. Vuelve a intentarlo.";

type ClientFactory = dyn Fn(&Url, Option<Instant>) -> Result<(ClientBuilder, Duration), String> + Send + Sync;

#[derive(Clone, Copy)]
pub enum MediaKind {
    Hls,
    File,
    TransportStream,
    Image,
}

impl MediaKind {
    fn mime_type(self) -> &'static str {
        match self {
            Self::Hls => "application/vnd.apple.mpegurl",
            Self::File => "video/mp4",
            Self::TransportStream => "video/mp2t",
            Self::Image => "application/octet-stream",
        }
    }
    fn is_file(self) -> bool { matches!(self, Self::File | Self::TransportStream) }
}

pub struct MediaSource {
    pub url: Url,
    pub kind: MediaKind,
    /// Private headers stay in Rust. Hop-by-hop headers and Range are rejected.
    pub headers: HeaderMap,
    /// Empty means only the initial URL's origin. Redirects never extend this set.
    pub credential_origins: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlaybackTicket {
    pub url: String,
    pub mime_type: String,
    pub session_id: String,
}

#[derive(Clone)]
struct Resource {
    url: Url,
    force_playlist: bool,
}

#[derive(Clone)]
struct Resources {
    entries: HashMap<u64, Resource>,
    by_url: HashMap<String, u64>,
    next: u64,
}

struct Session {
    token: String,
    headers: HeaderMap,
    credential_origins: HashSet<String>,
    resources: Mutex<Resources>,
    canceled: Arc<AtomicBool>,
    touched: Mutex<Instant>,
    verified_kind: OnceLock<MediaKind>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayAccess { Desktop, Cast }

impl RelayAccess {
    fn accepts_peer(self, address: SocketAddr) -> bool {
        match self {
            Self::Desktop => address.ip().is_loopback(),
            Self::Cast => matches!(address, SocketAddr::V4(peer) if peer.ip().is_private() || peer.ip().is_loopback()),
        }
    }
}

struct CachedClient {
    client: Client,
    until: Instant,
}

struct Inner {
    server: Arc<Server>,
    authority: String,
    access: RelayAccess,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    clients: Mutex<HashMap<String, CachedClient>>,
    factory: Arc<ClientFactory>,
    stopped: AtomicBool,
}

pub struct MediaProxy {
    inner: Arc<Inner>,
}

impl MediaProxy {
    /// The factory may set per-process DNS overrides while retaining the URL's
    /// hostname for SNI/TLS. The returned TTL controls connection-pool lifetime.
    pub fn start<F>(factory: F) -> Result<Self, String>
    where F: Fn(&Url, Option<Instant>) -> Result<(ClientBuilder, Duration), String> + Send + Sync + 'static {
        Self::start_bound(Ipv4Addr::LOCALHOST, RelayAccess::Desktop, factory)
    }

    /// Only called after an explicit Cast action. The caller chooses a local
    /// RFC1918 interface; binding a wildcard, public, or loopback IP is refused.
    pub fn start_cast<F>(bind_ip: Ipv4Addr, factory: F) -> Result<Self, String>
    where F: Fn(&Url, Option<Instant>) -> Result<(ClientBuilder, Duration), String> + Send + Sync + 'static {
        if !bind_ip.is_private() {
            return Err("Chromecast necesita una dirección IPv4 privada de esta red.".into());
        }
        Self::start_bound(bind_ip, RelayAccess::Cast, factory)
    }

    fn start_bound<F>(bind_ip: Ipv4Addr, access: RelayAccess, factory: F) -> Result<Self, String>
    where F: Fn(&Url, Option<Instant>) -> Result<(ClientBuilder, Duration), String> + Send + Sync + 'static {
        let address = SocketAddrV4::new(bind_ip, 0);
        let server = Arc::new(Server::http(address).map_err(|_| match access {
            RelayAccess::Desktop => "No se pudo abrir el reproductor local.",
            RelayAccess::Cast => "No se pudo abrir Chromecast en la dirección de red seleccionada.",
        }.to_string())?);
        let authority = server.server_addr().to_ip().ok_or("Dirección local no disponible.")?.to_string();
        let inner = Arc::new(Inner {
            server, authority, access, sessions: Mutex::new(HashMap::new()), clients: Mutex::new(HashMap::new()),
            factory: Arc::new(factory), stopped: AtomicBool::new(false),
        });
        for index in 0..WORKERS {
            let worker = Arc::clone(&inner);
            if thread::Builder::new().name(format!("xuper-media-{index}")).spawn(move || {
                while !worker.stopped.load(Ordering::Acquire) {
                    match worker.server.recv_timeout(Duration::from_millis(500)) {
                        Ok(Some(request)) => serve(&worker, request),
                        Ok(None) => {},
                        Err(_) => break,
                    }
                }
            }).is_err() {
                inner.stopped.store(true, Ordering::Release);
                for _ in 0..index { inner.server.unblock(); }
                return Err("No se pudo iniciar el reproductor local.".into());
            }
        }
        Ok(Self { inner })
    }

    pub fn register(&self, source: MediaSource) -> Result<PlaybackTicket, String> {
        self.require_desktop_registration()?;
        let kind = source.kind;
        self.insert_session(Self::prepare_session(source)?, kind)
    }

    /// Checks only a bounded prefix, not whether the platform can decode every
    /// track. No file extension or Content-Type can substitute for these bytes.
    pub fn register_verified(&self, source: MediaSource) -> Result<PlaybackTicket, String> {
        self.register_verified_until(source, Instant::now() + VERIFY_TIMEOUT)
    }

    /// The preparation deadline covers DNS, every redirect and the prefix body.
    /// Playback requests made with the returned ticket do not inherit it.
    pub fn register_verified_until(&self, source: MediaSource, deadline: Instant) -> Result<PlaybackTicket, String> {
        self.require_desktop_registration()?;
        remaining(deadline)?;
        let url = source.url.clone();
        let session = Self::prepare_session(source)?;
        let checked = (|| {
            let response = fetch(&self.inner, &session, reqwest::Method::GET, url,
                Some("bytes=0-4095"), None, Some(deadline))?;
            let status = response.status().as_u16();
            let total = match status {
                200 => response.content_length(),
                206 => probe_content_range(response.headers().get("Content-Range")
                    .and_then(|value| value.to_str().ok())).ok_or(UNKNOWN_MEDIA)?,
                _ => return Err("La fuente de reproducción no está disponible.".into()),
            };
            let mut prefix = Vec::with_capacity(PROBE_BYTES);
            let reader = CancelableReader { inner: response, canceled: Arc::clone(&session.canceled), cast_activity: None };
            reader.take(PROBE_BYTES as u64).read_to_end(&mut prefix)
                .map_err(|_| if Instant::now() >= deadline { VERIFY_TIMEOUT_MESSAGE } else { "La fuente de reproducción quedó incompleta." }.to_string())?;
            remaining(deadline)?;
            crate::native_player::diagnostic("media-container-probe",serde_json::json!({
                "status":status,"total_bytes":total,"prefix_bytes":prefix.len(),
                "magic":hex::encode(&prefix[..prefix.len().min(4)])
            }));
            let kind = detect_media(&prefix, total).ok_or(UNKNOWN_MEDIA)?;
            session.resources.lock().map_err(|_| "No se pudo preparar la reproducción.")?
                .entries.get_mut(&0).ok_or("No se pudo preparar la reproducción.")?
                .force_playlist = matches!(kind, MediaKind::Hls);
            session.verified_kind.set(kind).map_err(|_| "No se pudo verificar la reproducción.")?;
            Ok(kind)
        })();
        match checked {
            Ok(kind) => self.insert_session(session, kind),
            Err(error) => { session.canceled.store(true, Ordering::Release); Err(error) }
        }
    }

    fn require_desktop_registration(&self) -> Result<(), String> {
        if self.inner.access != RelayAccess::Desktop {
            return Err("Chromecast solo admite sesiones de reproducción ya verificadas.".into());
        }
        Ok(())
    }

    /// Export the existing verified root ticket from this desktop socket. The
    /// destination owns a snapshot of resources and credentials, a fresh token,
    /// and independent idle/cancellation state. No extra upstream request occurs.
    pub fn export_session_into(&self, cast_proxy: &MediaProxy, local_ticket: &PlaybackTicket) -> Result<PlaybackTicket, String> {
        if self.inner.access != RelayAccess::Desktop || cast_proxy.inner.access != RelayAccess::Cast
            || self.inner.stopped.load(Ordering::Acquire) || cast_proxy.inner.stopped.load(Ordering::Acquire) {
            return Err("La sesión no se puede transferir a Chromecast.".into());
        }
        let (session, kind) = {
            let sessions = self.inner.sessions.lock().map_err(|_| "No se pudo preparar Chromecast.")?;
            let original = sessions.get(&local_ticket.session_id).ok_or("La reproducción local ya no está disponible.")?;
            let expected_url = format!("http://{}/media/{}/0", self.inner.authority, original.token);
            if local_ticket.url != expected_url {
                return Err("El enlace de reproducción no pertenece a esta sesión local.".into());
            }
            if original.canceled.load(Ordering::Acquire)
                || original.touched.lock().map_err(|_| "No se pudo preparar Chromecast.")?.elapsed() >= SESSION_IDLE {
                return Err("La reproducción local ya no está disponible.".into());
            }
            let kind = *original.verified_kind.get().ok_or("Primero abre una reproducción verificada para enviarla a Chromecast.")?;
            let verified_kind = OnceLock::new();
            let _ = verified_kind.set(kind);
            let resources = original.resources.lock().map_err(|_| "No se pudo preparar Chromecast.")?.clone();
            let session = Arc::new(Session {
                token: uuid::Uuid::new_v4().simple().to_string(), headers: original.headers.clone(),
                credential_origins: original.credential_origins.clone(), resources: Mutex::new(resources),
                canceled: Arc::new(AtomicBool::new(false)), touched: Mutex::new(Instant::now()), verified_kind,
            });
            (session, kind)
        };
        cast_proxy.insert_session(session, kind)
    }

    fn prepare_session(source: MediaSource) -> Result<Arc<Session>, String> {
        validate_url(&source.url)?;
        for (name, value) in &source.headers {
            if matches!(name.as_str(), "host" | "connection" | "content-length" | "transfer-encoding" | "range" | "accept-encoding" | "upgrade" | "te" | "trailer" | "proxy-authorization" | "proxy-connection") || value.as_bytes().len() > 8192 {
                return Err("La fuente contiene un encabezado incompatible con el reproductor.".into());
            }
        }
        if source.headers.len() > 32 { return Err("La fuente contiene demasiados encabezados.".into()); }
        let mut origins = HashSet::new();
        for origin in source.credential_origins {
            let parsed = Url::parse(&origin).map_err(|_| "Origen de credenciales inválido.".to_string())?;
            validate_url(&parsed)?;
            if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
                return Err("El permiso de credenciales debe indicar un origen exacto.".into());
            }
            origins.insert(parsed.origin().ascii_serialization());
        }
        if origins.is_empty() { origins.insert(source.url.origin().ascii_serialization()); }
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut resources = Resources { entries: HashMap::new(), by_url: HashMap::new(), next: 1 };
        resources.by_url.insert(source.url.as_str().to_owned(), 0);
        resources.entries.insert(0, Resource { url: source.url, force_playlist: matches!(source.kind, MediaKind::Hls) });
        Ok(Arc::new(Session { token, headers: source.headers, credential_origins: origins,
            resources: Mutex::new(resources), canceled: Arc::new(AtomicBool::new(false)), touched: Mutex::new(Instant::now()), verified_kind: OnceLock::new() }))
    }

    fn insert_session(&self, session: Arc<Session>, kind: MediaKind) -> Result<PlaybackTicket, String> {
        let mut sessions = self.inner.sessions.lock().map_err(|_| "No se pudo preparar la reproducción.".to_string())?;
        sessions.retain(|_, session| {
            let keep = session.touched.lock().map(|time| time.elapsed() < SESSION_IDLE).unwrap_or(false);
            if !keep { session.canceled.store(true, Ordering::Release); }
            keep
        });
        if sessions.len() >= MAX_SESSIONS { return Err("Cierra el reproductor anterior antes de abrir otro.".into()); }
        let token = session.token.clone();
        sessions.insert(token.clone(), session);
        Ok(PlaybackTicket { url: format!("http://{}/media/{token}/0", self.inner.authority),
            mime_type: kind.mime_type().into(),
            session_id: token })
    }

    pub(crate) fn native_ticket(&self, session_id: &str) -> Result<PlaybackTicket, String> {
        let sessions = self.inner.sessions.lock().map_err(|_| "No se pudo consultar la reproducción local.")?;
        let session = sessions.get(session_id).ok_or("La reproducción local ya no está disponible.")?;
        if self.inner.access != RelayAccess::Desktop || session.canceled.load(Ordering::Acquire)
            || session.touched.lock().map_err(|_| "No se pudo consultar la reproducción local.")?.elapsed() >= SESSION_IDLE {
            return Err("La reproducción local ya no está disponible.".into());
        }
        let kind = session.verified_kind.get().ok_or("Primero prepara una fuente de vídeo válida.")?;
        Ok(PlaybackTicket { url: format!("http://{}/media/{}/0", self.inner.authority, session.token),
            mime_type: kind.mime_type().into(), session_id: session.token.clone() })
    }

    pub fn release(&self, session_id: &str) -> bool {
        self.inner.sessions.lock().ok().and_then(|mut sessions| sessions.remove(session_id)).map(|session| {
            session.canceled.store(true, Ordering::Release);
        }).is_some()
    }
}

impl Drop for MediaProxy {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::Release);
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            for session in sessions.values() { session.canceled.store(true, Ordering::Release); }
            sessions.clear();
        }
        for _ in 0..WORKERS { self.inner.server.unblock(); }
    }
}

fn validate_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.as_str().len() > 8192 {
        return Err("La fuente no contiene una URL HTTP o HTTPS compatible.".into());
    }
    Ok(())
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers().iter().find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|header| header.value.as_str())
}

fn cors_headers(access: RelayAccess) -> Vec<Header> {
    [
        ("Access-Control-Allow-Origin", if access == RelayAccess::Cast { "*" } else { UI_ORIGIN }),
        ("Access-Control-Expose-Headers", "Content-Length, Content-Range, Accept-Ranges, ETag"),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
    ].into_iter().map(|(name, value)| Header::from_bytes(name, value).unwrap()).collect()
}

fn simple(inner: &Inner, request: Request, status: u16, message: &'static str) {
    let mut response = Response::from_string(message).with_status_code(status);
    for header in cors_headers(inner.access) { response.add_header(header); }
    let _ = request.respond(response);
}

fn serve(inner: &Arc<Inner>, request: Request) {
    if std::env::var_os("XUPER_PLAYBACK_DIAGNOSTIC").is_some() && header(&request,"User-Agent").is_some_and(|ua|ua.starts_with("VLC")) {
        let record=serde_json::json!({"http_version":format!("{:?}",request.http_version()),
            "method":if *request.method()==Method::Get {"GET"} else if *request.method()==Method::Head {"HEAD"} else {"other"},
            "te":match header(&request,"TE") {None=>"absent",Some("trailers")=>"trailers",Some("identity")=>"identity",_=>"other"},
            "range_present":header(&request,"Range").is_some(),"host_matches":header(&request,"Host")==Some(inner.authority.as_str())});
        crate::native_player::diagnostic("relay-request",record);
    }
    // tiny_http buffers unknown-length responses for HTTP/1.0 or TE: identity.
    // The embedded player uses HTTP/1.1; reject those paths before any fetch.
    if *request.http_version() < (1, 1) || header(&request, "TE").is_some() {
        simple(inner, request, 505, "El reproductor requiere HTTP/1.1."); return;
    }
    if request.remote_addr().is_none_or(|address| !inner.access.accepts_peer(*address))
        || header(&request, "Host") != Some(inner.authority.as_str())
        || (inner.access == RelayAccess::Desktop && header(&request, "Origin").is_some_and(|origin| origin != UI_ORIGIN)) {
        simple(inner, request, 403, "Solicitud no permitida."); return;
    }
    let route: Vec<&str> = request.url().split('/').collect();
    if route.len() != 4 || route[1] != "media" || route[2].len() != 32 || !route[2].bytes().all(|c| c.is_ascii_hexdigit()) {
        simple(inner, request, 404, "Recurso no disponible."); return;
    }
    let session = inner.sessions.lock().ok().and_then(|sessions| sessions.get(route[2]).cloned());
    let Some(session) = session else { simple(inner, request, 404, "Recurso no disponible."); return; };
    if session.canceled.load(Ordering::Acquire) { simple(inner, request, 410, "Reproducción cerrada."); return; }
    let resource_id = match route[3].parse::<u64>() { Ok(id) => id, Err(_) => { simple(inner, request, 404, "Recurso no disponible."); return; } };
    let resource = session.resources.lock().ok().and_then(|resources| resources.entries.get(&resource_id).map(|resource| (resource.url.clone(), resource.force_playlist)));
    let Some((url, force_playlist)) = resource else { simple(inner, request, 404, "Recurso no disponible."); return; };
    let expired = session.touched.lock().map(|mut touched| {
        if inner.access == RelayAccess::Cast && touched.elapsed() >= SESSION_IDLE { return true; }
        *touched = Instant::now();
        false
    }).unwrap_or(true);
    if expired {
        session.canceled.store(true, Ordering::Release);
        if let Ok(mut sessions) = inner.sessions.lock() { sessions.remove(&session.token); }
        simple(inner, request, 410, "Reproducción cerrada."); return;
    }
    if *request.method() == Method::Options {
        let mut response = Response::empty(204);
        for header in cors_headers(inner.access) { response.add_header(header); }
        response.add_header(Header::from_bytes("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS").unwrap());
        response.add_header(Header::from_bytes("Access-Control-Allow-Headers", "Range, If-Range").unwrap());
        if inner.access == RelayAccess::Cast && header(&request, "Access-Control-Request-Private-Network") == Some("true") {
            response.add_header(Header::from_bytes("Access-Control-Allow-Private-Network", "true").unwrap());
        }
        let _ = request.respond(response); return;
    }
    if !matches!(request.method(), Method::Get | Method::Head) { simple(inner, request, 405, "Método no admitido."); return; }
    let range = header(&request, "Range").map(str::to_owned);
    if range.as_deref().is_some_and(|value| !valid_range(value)) { simple(inner, request, 416, "Rango no admitido."); return; }
    let if_range = header(&request, "If-Range").filter(|value| value.len() <= 256).map(str::to_owned);
    let method = if *request.method() == Method::Head { reqwest::Method::HEAD } else { reqwest::Method::GET };
    // A partial playlist cannot be rewritten safely. Both players receive the
    // complete rewritten representation of known playlists; byte ranges on
    // media segments and video files still pass through unchanged.
    let whole_playlist = force_playlist;
    crate::native_player::diagnostic("relay-fetch-start",serde_json::json!({"range_present":range.is_some()}));
    let response = fetch(inner, &session, method, url,
        if whole_playlist { None } else { range.as_deref() },
        if whole_playlist { None } else { if_range.as_deref() }, None);
    let response = match response { Ok(response) => response, Err(_) => { crate::native_player::diagnostic("relay-fetch-error",serde_json::json!({})); simple(inner, request, 502, "No se pudo acceder a la fuente de reproducción."); return; } };
    crate::native_player::diagnostic("relay-fetch-response",serde_json::json!({"status":response.status().as_u16()}));
    if session.canceled.load(Ordering::Acquire) { simple(inner, request, 410, "Reproducción cerrada."); return; }
    let status = response.status().as_u16();
    if !matches!(status, 200 | 206 | 304 | 416) { simple(inner, request, status, "La fuente no está disponible."); return; }
    let content_type = response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("application/octet-stream").to_owned();
    // Container bytes already verified for this root take precedence over a
    // misleading upstream MIME or suffix. This does not classify child resources.
    let verified_file = resource_id == 0 && session.verified_kind.get().is_some_and(|kind| kind.is_file());
    let is_playlist = !verified_file && (force_playlist || content_type.to_ascii_lowercase().contains("mpegurl") || response.url().path().to_ascii_lowercase().ends_with(".m3u8"));
    if is_playlist && status == 206 {
        simple(inner, request, 502, "La fuente devolvió una lista de reproducción parcial."); return;
    }
    if is_playlist && status == 200 && *request.method() == Method::Get {
        let base = response.url().clone();
        let mut bytes = Vec::new();
        let reader = CancelableReader { inner: response, canceled: Arc::clone(&session.canceled),
            cast_activity: (inner.access == RelayAccess::Cast).then(|| Arc::clone(&session)) };
        if reader.take((MAX_PLAYLIST + 1) as u64).read_to_end(&mut bytes).is_err() || bytes.len() > MAX_PLAYLIST {
            simple(inner, request, 502, "La lista de reproducción excede el tamaño permitido o quedó incompleta."); return;
        }
        let text = match std::str::from_utf8(&bytes) { Ok(text) => text, Err(_) => { simple(inner, request, 502, "Lista de reproducción no válida."); return; } };
        let rewritten = rewrite_playlist(inner, &session, &base, text);
        match rewritten {
            Ok(body) => {
                let mut response = Response::from_string(body);
                for header in cors_headers(inner.access) { response.add_header(header); }
                response.add_header(Header::from_bytes("Content-Type", "application/vnd.apple.mpegurl").unwrap());
                let _ = request.respond(response);
            }
            Err(_) => simple(inner, request, 502, "La lista usa un formato no compatible con el reproductor local."),
        }
        return;
    }
    let mut headers = cors_headers(inner.access);
    let file_response = verified_file && matches!(status, 200 | 206);
    if file_response { headers.push(Header::from_bytes("Content-Type", session.verified_kind.get().unwrap().mime_type()).unwrap()); }
    for name in ["content-type", "content-range", "accept-ranges", "etag", "last-modified", "content-encoding"] {
        if file_response && name == "content-type" { continue; }
        if let Some(value) = response.headers().get(name) {
            if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) { headers.push(header); }
        }
    }
    let length = response.headers().get(CONTENT_LENGTH).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<usize>().ok());
    let reader = CancelableReader { inner: response, canceled: Arc::clone(&session.canceled),
        cast_activity: (inner.access == RelayAccess::Cast).then(|| Arc::clone(&session)) };
    let response = Response::new(StatusCode(status), headers, reader, length, None).with_chunked_threshold(usize::MAX);
    let _ = request.respond(response);
}

fn valid_range(value: &str) -> bool {
    let Some(value) = value.strip_prefix("bytes=") else { return false; };
    let Some((start, end)) = value.split_once('-') else { return false; };
    if start.is_empty() && end.is_empty() { return false; }
    let integer = |text: &str| text.is_empty() || text.parse::<u64>().is_ok();
    integer(start) && integer(end) && (start.is_empty() || end.is_empty() || start.parse::<u64>().unwrap() <= end.parse::<u64>().unwrap())
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline.checked_duration_since(Instant::now()).filter(|time| !time.is_zero())
        .ok_or_else(|| VERIFY_TIMEOUT_MESSAGE.to_string())
}

// A 206 probe must describe the beginning of the object. Its full size may be
// unknown; the optional total is used to reject impossible MP4 box lengths.
fn probe_content_range(value: Option<&str>) -> Option<Option<u64>> {
    let (range, total) = value?.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end) = (start.parse::<u64>().ok()?, end.parse::<u64>().ok()?);
    if start != 0 || end < start { return None; }
    if total == "*" { return Some(None); }
    let total = total.parse::<u64>().ok()?;
    (total > end).then_some(Some(total))
}

fn detect_media(prefix: &[u8], total: Option<u64>) -> Option<MediaKind> {
    let text = prefix.strip_prefix(b"\xef\xbb\xbf").unwrap_or(prefix);
    if let Some(rest) = text.strip_prefix(b"#EXTM3U") {
        if rest.is_empty() || matches!(rest.first(), Some(b'\r' | b'\n')) { return Some(MediaKind::Hls); }
    }
    if valid_mp4_prefix(prefix, total) { return Some(MediaKind::File); }
    valid_ts_prefix(prefix).then_some(MediaKind::TransportStream)
}

// A transport-stream file has no single magic string. Require five complete
// synchronized 188-byte packets with valid transport/adaptation headers. A
// misleading extension or MIME, or a lone 0x47 byte, cannot authorize a file.
fn valid_ts_prefix(prefix: &[u8]) -> bool {
    prefix.len() >= 5 * 188 && prefix.chunks_exact(188).take(5).all(|packet| {
        if packet[0] != 0x47 || packet[1] & 0x80 != 0 { return false; }
        match (packet[3] >> 4) & 3 {
            0 => false,
            1 => true,
            2 => packet[4] == 183,
            _ => packet[4] <= 182,
        }
    })
}

// Inspect complete box headers and the entire ftyp brand table. A large mdat or
// moov may continue beyond the prefix; this is container identification, not a
// decoder test. All offsets are checked, and at most 64 headers are inspected.
fn valid_mp4_prefix(prefix: &[u8], total: Option<u64>) -> bool {
    let complete = total.is_some_and(|length| length <= prefix.len() as u64);
    let mut offset = 0usize;
    let mut ftyp = false;
    let mut media = false;
    for _ in 0..64 {
        if prefix.len().saturating_sub(offset) < 8 {
            return ftyp && media && (offset == prefix.len() || !complete);
        }
        let size = u32::from_be_bytes(prefix[offset..offset + 4].try_into().unwrap());
        let name = &prefix[offset + 4..offset + 8];
        let header_size = if size == 1 { 16usize } else { 8usize };
        if prefix.len() - offset < header_size { return ftyp && media && !complete; }
        let length = match size {
            0 => total.and_then(|length| length.checked_sub(offset as u64)),
            1 => Some(u64::from_be_bytes(prefix[offset + 8..offset + 16].try_into().unwrap())),
            _ => Some(size as u64),
        };
        if length.is_some_and(|length| length < header_size as u64) { return false; }
        let end = length.and_then(|length| (offset as u64).checked_add(length));
        if length.is_some() && end.is_none() { return false; }
        if total.zip(end).is_some_and(|(total, end)| end > total) { return false; }
        if name == b"ftyp" {
            let Some(end) = end.filter(|end| *end <= prefix.len() as u64).map(|end| end as usize) else { return false; };
            let brands = &prefix[offset + header_size..end];
            if ftyp || brands.len() < 8 || (brands.len() - 8) % 4 != 0 { return false; }
            let known = |brand: &[u8]| matches!(brand, b"isom" | b"iso2" | b"iso3" | b"iso4" | b"iso5" | b"iso6" | b"iso7" | b"iso8" | b"iso9" | b"mp41" | b"mp42" | b"avc1" | b"hvc1" | b"hev1" | b"dash" | b"cmfc" | b"cmfs");
            if !known(&brands[..4]) && !brands[8..].chunks_exact(4).any(known) { return false; }
            ftyp = true;
        } else if matches!(name, b"moov" | b"mdat" | b"moof") {
            if !ftyp || length.is_some_and(|length| length <= header_size as u64) { return false; }
            media = true;
        } else if !ftyp && !matches!(name, b"free" | b"skip" | b"wide") {
            return false;
        }
        let Some(end) = end else { return ftyp && media && size == 0; };
        if end > prefix.len() as u64 { return ftyp && media; }
        offset = end as usize;
    }
    false
}

fn client(inner: &Inner, url: &Url, deadline: Option<Instant>) -> Result<Client, String> {
    if let Some(deadline) = deadline { remaining(deadline)?; }
    let origin = url.origin().ascii_serialization();
    if let Some(client) = inner.clients.lock().ok().and_then(|clients| clients.get(&origin).filter(|entry| entry.until > Instant::now()).map(|entry| entry.client.clone())) {
        return Ok(client);
    }
    let (builder, ttl) = (inner.factory)(url, deadline)?;
    if let Some(deadline) = deadline { remaining(deadline)?; }
    // Redirects are followed manually to apply the credential-origin policy.
    let client = builder.redirect(Policy::none()).timeout(Duration::from_secs(15)).connect_timeout(Duration::from_secs(10)).build()
        .map_err(|_| "No se pudo preparar la conexión de video.".to_string())?;
    if !ttl.is_zero() {
        if let Ok(mut clients) = inner.clients.lock() {
            clients.retain(|_, entry| entry.until > Instant::now());
            if clients.len() >= MAX_CLIENTS { clients.clear(); }
            clients.insert(origin, CachedClient { client: client.clone(), until: Instant::now() + ttl.min(Duration::from_secs(300)) });
        }
    }
    Ok(client)
}

fn fetch(inner: &Inner, session: &Session, method: reqwest::Method, mut url: Url, range: Option<&str>, if_range: Option<&str>, deadline: Option<Instant>) -> Result<UpstreamResponse, String> {
    for _ in 0..6 {
        validate_url(&url)?;
        if session.canceled.load(Ordering::Acquire) { return Err("Reproducción cerrada.".into()); }
        let client = client(inner, &url, deadline)?;
        let mut request = client.request(method.clone(), url.clone()).header(ACCEPT_ENCODING, "identity");
        // Unlike the blocking client's per-read timeout, this explicit request
        // timeout also reaches reqwest's async body and is a total time limit.
        if let Some(deadline) = deadline { request = request.timeout(remaining(deadline)?); }
        if session.credential_origins.contains(&url.origin().ascii_serialization()) { request = request.headers(session.headers.clone()); }
        if let Some(range) = range { request = request.header(RANGE, range); }
        if let Some(value) = if_range { request = request.header("If-Range", value); }
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => {
                if error.is_connect() {
                    if let Ok(mut clients) = inner.clients.lock() { clients.remove(&url.origin().ascii_serialization()); }
                }
                return Err("No se pudo conectar con la fuente de video.".into());
            }
        };
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            let location = response.headers().get(LOCATION).and_then(|value| value.to_str().ok()).ok_or("Redirección de video inválida.")?;
            url = url.join(location).map_err(|_| "Redirección de video inválida.".to_string())?;
        } else { return Ok(response); }
    }
    Err("La fuente redirigió demasiadas veces.".into())
}

fn register_resource(inner: &Inner, session: &Session, base: &Url, reference: &str, force_playlist: bool) -> Result<String, String> {
    if reference.contains("{$") { return Err("Variables HLS aún no admitidas.".into()); }
    let mut url = base.join(reference).map_err(|_| "URI HLS inválida.".to_string())?;
    validate_url(&url)?;
    url.set_fragment(None);
    let mut resources = session.resources.lock().map_err(|_| "Lista HLS no disponible.".to_string())?;
    let id = if let Some(id) = resources.by_url.get(url.as_str()).copied() {
        if force_playlist { if let Some(resource) = resources.entries.get_mut(&id) { resource.force_playlist = true; } }
        id
    } else {
        if resources.entries.len() >= MAX_RESOURCES { return Err("La lista contiene demasiados recursos.".into()); }
        let id = resources.next; resources.next += 1;
        resources.by_url.insert(url.as_str().to_owned(), id);
        resources.entries.insert(id, Resource { url, force_playlist });
        id
    };
    Ok(format!("http://{}/media/{}/{id}", inner.authority, session.token))
}

fn rewrite_playlist(inner: &Inner, session: &Session, base: &Url, text: &str) -> Result<String, String> {
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") { return Err("No es una lista HLS.".into()); }
    let mut result = String::with_capacity(text.len());
    let mut next_playlist = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        if line.starts_with("#EXT-X-CONTENT-STEERING") || line.starts_with("#EXT-X-DEFINE") { return Err("Extensión HLS no admitida.".into()); }
        if line.trim().is_empty() { result.push('\n'); continue; }
        if !line.starts_with('#') {
            result.push_str(&register_resource(inner, session, base, line.trim(), next_playlist)?);
            next_playlist = false;
        } else {
            if line.starts_with("#EXT-X-STREAM-INF:") { next_playlist = true; }
            let attribute_playlist = ["#EXT-X-MEDIA:", "#EXT-X-I-FRAME-STREAM-INF:", "#EXT-X-RENDITION-REPORT:"].iter().any(|tag| line.starts_with(tag));
            let mut remainder = line;
            while let Some(position) = remainder.find("URI=\"") {
                let value_start = position + 5;
                let value_end = remainder[value_start..].find('"').map(|position| value_start + position).ok_or("Atributo HLS inválido.")?;
                result.push_str(&remainder[..value_start]);
                result.push_str(&register_resource(inner, session, base, &remainder[value_start..value_end], attribute_playlist)?);
                result.push('"');
                remainder = &remainder[value_end + 1..];
            }
            result.push_str(remainder);
        }
        result.push('\n');
        if result.len() > MAX_REWRITTEN_PLAYLIST { return Err("La lista reescrita excede el tamaño permitido.".into()); }
    }
    Ok(result)
}

struct CancelableReader<R> {
    inner: R,
    canceled: Arc<AtomicBool>,
    // A large MP4 can use one GET for hours. Positive reads count as Cast
    // activity so the idle limit does not expire an ongoing transmission.
    cast_activity: Option<Arc<Session>>,
}

impl<R: Read> Read for CancelableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.canceled.load(Ordering::Acquire) { return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "Reproducción cerrada")); }
        let limit = buffer.len().min(64 * 1024);
        let read = self.inner.read(&mut buffer[..limit])?;
        if read > 0 {
            if let Some(session) = &self.cast_activity {
                if let Ok(mut touched) = session.touched.lock() { *touched = Instant::now(); }
            }
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Cursor, Write}, net::TcpListener, sync::mpsc};

    fn local_client() -> Client { Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap() }
    fn proxy() -> MediaProxy { MediaProxy::start(|_, _| Ok((Client::builder().no_proxy(), Duration::from_secs(60)))).unwrap() }
    fn source(url: &str, kind: MediaKind) -> MediaSource {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Auth", "synthetic-private-token".parse().unwrap());
        MediaSource { url: Url::parse(url).unwrap(), kind, headers, credential_origins: vec![] }
    }

    // Synthetic ISO BMFF boxes identify a container; they deliberately do not
    // claim to contain decodable tracks or any provider media.
    fn mp4_prefix(total: u32, prefix_length: usize) -> Vec<u8> {
        let mut bytes = Vec::from(&b"\0\0\0\x18ftypisom\0\0\0\0isommp42"[..]);
        bytes.extend_from_slice(&(total - 24).to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.resize(prefix_length, 0);
        bytes
    }

    // Serve only a small prefix while advertising a much larger object. The
    // rest never arrives until the caller explicitly releases the fixture.
    fn held_body(prefix: Vec<u8>, total: usize) -> (String, mpsc::Sender<()>, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/untyped", listener.local_addr().unwrap());
        let (release, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < accept_deadline => thread::sleep(Duration::from_millis(10)),
                    Err(error) => panic!("Local fixture did not receive a connection: {error}"),
                }
            };
            // Winsock can inherit the listener's nonblocking mode. Request
            // reads below deliberately use blocking I/O with a finite timeout.
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16 * 1024);
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n").unwrap();
            socket.write_all(&prefix).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
            String::from_utf8(request).unwrap()
        });
        (url, release, worker)
    }

    #[test]
    fn ts_detection_requires_synchronized_valid_packet_headers() {
        let bytes=ts_fixture();
        assert!(matches!(detect_media(&bytes,Some(bytes.len() as u64)),Some(MediaKind::TransportStream)));
        assert!(detect_media(&bytes[..939],Some(bytes.len() as u64)).is_none());
        for (offset,value) in [(188,0),(189,0x80),(191,0),(191,0x30),(192,183)] {
            let mut invalid=bytes.clone();
            // The last case is an overflowing adaptation+payload field.
            if offset==192 { invalid[191]=0x30; }
            invalid[offset]=value;
            assert!(detect_media(&invalid,Some(invalid.len() as u64)).is_none());
        }
        assert!(detect_media(b"<html>not a video</html>",None).is_none());
    }

    fn ts_fixture() -> Vec<u8> {
        let mut bytes=vec![0xff;188*30];
        for (index,packet) in bytes.chunks_exact_mut(188).enumerate() {
            packet[..4].copy_from_slice(&[0x47,0x40,0x11,0x10|(index as u8&15)]);
        }
        bytes
    }

    #[test]
    fn verified_ts_preserves_ranges_and_mime_with_a_misleading_playlist_url() {
        let upstream=Server::http("127.0.0.1:0").unwrap();
        let url=format!("http://{}/mislabelled.m3u8",upstream.server_addr());
        let bytes=ts_fixture();let expected=bytes[188..376].to_vec();
        let worker=thread::spawn(move|| {
            for index in 0..2 {
                let request=upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                let (start,end)=if index==0 {(0,4096)}else{(188,376)};
                assert_eq!(header(&request,"Range"),Some(if index==0 {"bytes=0-4095"}else{"bytes=188-375"}));
                request.respond(Response::from_data(bytes[start..end].to_vec()).with_status_code(206)
                    .with_header(Header::from_bytes("Content-Type","application/vnd.apple.mpegurl").unwrap())
                    .with_header(Header::from_bytes("Content-Range",format!("bytes {}-{}/{}",start,end-1,bytes.len())).unwrap())).unwrap();
            }
        });
        let proxy=proxy();
        let ticket=proxy.register_verified(source(&url,MediaKind::Hls)).unwrap();
        assert_eq!(ticket.mime_type,"video/mp2t");
        assert_eq!(proxy.native_ticket(&ticket.session_id).unwrap().mime_type,"video/mp2t");
        let response=local_client().get(&ticket.url).header("Range","bytes=188-375").send().unwrap();
        assert_eq!(response.status(),206);
        assert_eq!(response.headers()["Content-Type"],"video/mp2t");
        assert_eq!(response.headers()["Content-Range"],"bytes 188-375/5640");
        assert_eq!(response.bytes().unwrap().as_ref(),expected.as_slice());
        worker.join().unwrap();
    }

    #[test]
    fn verifies_hls_and_mp4_from_bytes_with_misleading_hints() {
        let cases = [
            (b"\xef\xbb\xbf#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nsegment.ts\n".to_vec(), MediaKind::File, 200, "video/mp4", "application/vnd.apple.mpegurl", true),
            (mp4_prefix(512, 512), MediaKind::Hls, 206, "text/plain", "video/mp4", false),
        ];
        for (body, hint, status, content_type, expected_mime, playlist) in cases {
            let upstream = Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/no-extension", upstream.server_addr());
            let worker = thread::spawn(move || {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(header(&request, "Range"), Some("bytes=0-4095"));
                assert_eq!(header(&request, "Content-Auth"), Some("synthetic-private-token"));
                let mut response = Response::from_data(body).with_status_code(status)
                    .with_header(Header::from_bytes("Content-Type", content_type).unwrap());
                if status == 206 { response.add_header(Header::from_bytes("Content-Range", "bytes 0-511/512").unwrap()); }
                request.respond(response).unwrap();
            });
            let proxy = proxy();
            let ticket = proxy.register_verified(source(&url, hint)).unwrap();
            assert_eq!(ticket.mime_type, expected_mime);
            assert!(!ticket.url.contains("synthetic"));
            let session = proxy.inner.sessions.lock().unwrap().get(&ticket.session_id).unwrap().clone();
            assert_eq!(session.resources.lock().unwrap().entries[&0].force_playlist, playlist);
            worker.join().unwrap();
        }
    }

    #[test]
    fn rejects_unrecognized_and_malformed_probe_responses_without_registering() {
        let mut overflow = mp4_prefix(512, 24);
        overflow.extend_from_slice(&1u32.to_be_bytes());
        overflow.extend_from_slice(b"mdat");
        overflow.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut bad_box = mp4_prefix(512, 512);
        bad_box[24..28].copy_from_slice(&4u32.to_be_bytes());
        let cases = [
            (b"<!doctype html><html>Access required</html>".to_vec(), 200, None),
            (b"{\"error\":\"not a video\"}".to_vec(), 200, None),
            (b"#EXTM3U-fake\n".to_vec(), 200, None),
            (mp4_prefix(512, 24), 200, None),
            (b"\xff\xff\xff\xffftypisom\0\0\0\0".to_vec(), 200, None),
            (overflow, 200, None),
            (bad_box, 200, None),
            (mp4_prefix(512, 512), 206, None),
            (mp4_prefix(512, 512), 206, Some("bytes 7-518/600")),
            (mp4_prefix(512, 512), 206, Some("bytes 0-511/511")),
        ];
        let proxy = proxy();
        for (body, status, content_range) in cases {
            let upstream = Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/misleading.mp4", upstream.server_addr());
            let worker = thread::spawn(move || {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                let mut response = Response::from_data(body).with_status_code(status)
                    .with_header(Header::from_bytes("Content-Type", "video/mp4").unwrap());
                if let Some(value) = content_range { response.add_header(Header::from_bytes("Content-Range", value).unwrap()); }
                request.respond(response).unwrap();
            });
            assert_eq!(proxy.register_verified(source(&url, MediaKind::File)).unwrap_err(), UNKNOWN_MEDIA);
            assert!(proxy.inner.sessions.lock().unwrap().is_empty());
            worker.join().unwrap();
        }
    }

    #[test]
    fn ignored_range_is_bounded_and_only_recognized_bytes_are_accepted() {
        const TOTAL: usize = 64 * 1024 * 1024;
        let mut html = b"<html>This is an error page, not a media container.</html>".to_vec();
        html.resize(16 * 1024, b' ');
        for (prefix, valid) in [(mp4_prefix(TOTAL as u32, 16 * 1024), true), (html, false)] {
            let (url, release, worker) = held_body(prefix, TOTAL);
            let proxy = proxy();
            let before = Instant::now();
            let result = proxy.register_verified_until(source(&url, MediaKind::File), before + Duration::from_secs(2));
            let elapsed = before.elapsed();
            let _ = release.send(());
            let request = worker.join().unwrap();
            assert!(request.to_ascii_lowercase().contains("range: bytes=0-4095\r\n"));
            assert!(elapsed < Duration::from_secs(1), "A bounded prefix must not wait for the advertised body");
            if valid { assert_eq!(result.unwrap().mime_type, "video/mp4"); }
            else { assert_eq!(result.unwrap_err(), UNKNOWN_MEDIA); }
            assert_eq!(proxy.inner.sessions.lock().unwrap().len(), usize::from(valid));
        }
    }

    #[test]
    fn verification_deadline_covers_redirects_factory_and_stalled_body() {
        let first = Server::http("127.0.0.1:0").unwrap();
        let initial = format!("http://{}/redirect", first.server_addr());
        let (redirected, release, body_worker) = held_body(b"#EXTM3U\n".to_vec(), 1024 * 1024);
        let redirect_worker = thread::spawn(move || {
            let request = first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(header(&request, "Content-Auth"), Some("synthetic-private-token"));
            thread::sleep(Duration::from_millis(150));
            request.respond(Response::empty(302).with_header(Header::from_bytes("Location", redirected).unwrap())).unwrap();
        });
        let deadline = Instant::now() + Duration::from_millis(600);
        let (seen, observations) = mpsc::channel();
        let proxy = MediaProxy::start(move |_, supplied_deadline| {
            seen.send(supplied_deadline).unwrap();
            Ok((Client::builder().no_proxy(), Duration::ZERO))
        }).unwrap();
        let before = Instant::now();
        let result = proxy.register_verified_until(source(&initial, MediaKind::Hls), deadline);
        let elapsed = before.elapsed();
        let _ = release.send(());
        let request = body_worker.join().unwrap();
        redirect_worker.join().unwrap();
        assert!(result.is_err());
        assert!(elapsed < Duration::from_millis(1500), "The body must share the preparation deadline");
        assert!(!request.to_ascii_lowercase().contains("content-auth:"));
        assert_eq!(observations.try_iter().collect::<Vec<_>>(), vec![Some(deadline), Some(deadline)]);
        assert!(proxy.inner.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn streams_range_and_preserves_206_headers() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/movie.mp4", upstream.server_addr());
        let (sent, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(header(&request, "Ranger-Id"), Some("synthetic-player-identity"));
            assert_eq!(header(&request, "App"), Some("com.msandroid.mobile"));
            assert_eq!(header(&request, "App-Version"), Some("60291"));
            assert_eq!(header(&request, "User-Agent"), Some("Ranger/4.9.5-ebebe425"));
            sent.send((header(&request, "Range").map(str::to_owned), header(&request, "Content-Auth").map(str::to_owned))).unwrap();
            let response = Response::from_data(b"2345".to_vec()).with_status_code(206)
                .with_header(Header::from_bytes("Content-Range", "bytes 2-5/10").unwrap())
                .with_header(Header::from_bytes("Accept-Ranges", "bytes").unwrap())
                .with_header(Header::from_bytes("Content-Type", "video/mp4").unwrap());
            request.respond(response).unwrap();
        });
        let proxy = proxy();
        let mut media = source(&url, MediaKind::File);
        for (name, value) in [("Ranger-Id", "synthetic-player-identity"), ("App", "com.msandroid.mobile"),
            ("App-Version", "60291"), ("User-Agent", "Ranger/4.9.5-ebebe425")] {
            media.headers.insert(reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
        }
        let ticket = proxy.register(media).unwrap();
        let response = local_client().get(&ticket.url).header("Range", "bytes=2-5").header("Origin", UI_ORIGIN).send().unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["Content-Range"], "bytes 2-5/10");
        assert_eq!(response.headers()["Access-Control-Allow-Origin"], UI_ORIGIN);
        assert_eq!(response.bytes().unwrap().as_ref(), b"2345");
        assert_eq!(received.recv().unwrap(), (Some("bytes=2-5".into()), Some("synthetic-private-token".into())));
        worker.join().unwrap();
    }

    #[test]
    fn rewrites_hls_variants_keys_maps_and_segments_without_exposing_credentials() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hls/main.m3u8?private=synthetic", upstream.server_addr());
        let worker = thread::spawn(move || {
            let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            request.respond(Response::from_string("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../key.bin\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-STREAM-INF:BANDWIDTH=100\nvariant/index.m3u8\n#EXTINF:3,\nsegment.ts\n")).unwrap();
        });
        let proxy = proxy();
        let ticket = proxy.register(source(&url, MediaKind::Hls)).unwrap();
        let text = local_client().get(&ticket.url).send().unwrap().text().unwrap();
        assert!(text.starts_with("#EXTM3U\n"));
        assert!(!text.contains("synthetic"));
        assert!(!text.contains("key.bin"));
        assert_eq!(text.matches(&format!("http://{}/media/", proxy.inner.authority)).count(), 4);
        let session = proxy.inner.sessions.lock().unwrap().get(&ticket.session_id).unwrap().clone();
        let resources = session.resources.lock().unwrap();
        assert!(resources.by_url.keys().any(|url| url.ends_with("/key.bin")));
        assert!(resources.by_url.keys().any(|url| url.ends_with("/hls/variant/index.m3u8")));
        worker.join().unwrap();
    }

    #[test]
    fn desktop_playlist_range_gets_complete_rewrite_but_segment_keeps_its_range() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let authority = upstream.server_addr().to_string();
        let url = format!("http://{authority}/playlist");
        const PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:3,\nsegment.ts?token=synthetic\n#EXT-X-ENDLIST\n";
        let worker = thread::spawn(move || {
            for index in 0..3 {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                if index < 2 {
                    assert_eq!(request.url(), "/playlist");
                    assert_eq!(header(&request, "Range"), if index == 0 { Some("bytes=0-4095") } else { None });
                    assert!(header(&request, "If-Range").is_none());
                    request.respond(Response::from_string(PLAYLIST)).unwrap();
                } else {
                    assert_eq!(request.url(), "/segment.ts?token=synthetic");
                    assert_eq!(header(&request, "Range"), Some("bytes=2-5"));
                    assert_eq!(header(&request, "If-Range"), Some("\"segment-v1\""));
                    request.respond(Response::from_data(b"2345".to_vec()).with_status_code(206)
                        .with_header(Header::from_bytes("Content-Range", "bytes 2-5/10").unwrap())
                        .with_header(Header::from_bytes("Content-Type", "video/mp2t").unwrap())).unwrap();
                }
            }
        });
        let proxy = proxy();
        let ticket = proxy.register_verified(source(&url, MediaKind::Hls)).unwrap();
        let client = local_client();
        let response = client.get(&ticket.url).header("Range", "bytes=0-31")
            .header("If-Range", "\"playlist-v1\"").send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Content-Type"], "application/vnd.apple.mpegurl");
        assert!(response.headers().get("Content-Range").is_none());
        let playlist = response.text().unwrap();
        assert!(!playlist.contains(&authority));
        assert!(!playlist.contains("synthetic"));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
        let segment = playlist.lines().find(|line| line.starts_with("http://")).unwrap();
        let response = client.get(segment).header("Range", "bytes=2-5")
            .header("If-Range", "\"segment-v1\"").send().unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["Content-Type"], "video/mp2t");
        assert_eq!(response.headers()["Content-Range"], "bytes 2-5/10");
        assert_eq!(response.bytes().unwrap().as_ref(), b"2345");
        worker.join().unwrap();
    }

    #[test]
    fn desktop_rejects_partial_hls_even_when_only_response_mime_identifies_it() {
        for hint in [MediaKind::Hls, MediaKind::File] {
            let upstream = Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/opaque", upstream.server_addr());
            let worker = thread::spawn(move || {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert!(header(&request, "Range").is_none());
                request.respond(Response::from_string("#EXTM3U\n#EXTINF:3,\nhttps://example.invalid/private-segment\n")
                    .with_status_code(206)
                    .with_header(Header::from_bytes("Content-Type", "application/vnd.apple.mpegurl").unwrap())
                    .with_header(Header::from_bytes("Content-Range", "bytes 0-62/512").unwrap())).unwrap();
            });
            let proxy = proxy();
            let ticket = proxy.register(source(&url, hint)).unwrap();
            let response = local_client().get(&ticket.url).send().unwrap();
            assert_eq!(response.status(), 502);
            let message = response.text().unwrap();
            assert!(message.contains("lista de reproducción parcial"));
            assert!(!message.contains("example.invalid"));
            assert!(!message.contains("#EXTM3U"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn verified_mp4_normalizes_full_range_and_head_mime_without_changing_unverified_sources() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        // Both the suffix and MIME are deliberately misleading; only the probe
        // bytes establish that this root is an MP4 container.
        let url = format!("http://{}/mislabelled.m3u8", upstream.server_addr());
        let unverified_url = format!("http://{}/unverified", upstream.server_addr());
        let bytes = mp4_prefix(512, 512);
        let expected = bytes.clone();
        let worker = thread::spawn(move || {
            for index in 0..5 {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                if index == 4 {
                    assert_eq!(request.url(), "/unverified");
                    request.respond(Response::from_string("unverified response")
                        .with_header(Header::from_bytes("Content-Type", "text/plain").unwrap())).unwrap();
                } else if index == 2 {
                    assert_eq!(header(&request, "Range"), Some("bytes=24-31"));
                    request.respond(Response::from_data(bytes[24..32].to_vec()).with_status_code(206)
                        .with_header(Header::from_bytes("Content-Range", "bytes 24-31/512").unwrap())
                        .with_header(Header::from_bytes("Accept-Ranges", "bytes").unwrap())
                        .with_header(Header::from_bytes("Content-Type", "application/vnd.apple.mpegurl").unwrap())).unwrap();
                } else {
                    assert_eq!(header(&request, "Range"), if index == 0 { Some("bytes=0-4095") } else { None });
                    assert_eq!(*request.method(), if index == 3 { Method::Head } else { Method::Get });
                    request.respond(Response::from_data(bytes.clone())
                        .with_header(Header::from_bytes("Content-Type", if index == 3 { "application/octet-stream" } else { "text/plain" }).unwrap())).unwrap();
                }
            }
        });
        let proxy = proxy();
        let ticket = proxy.register_verified(source(&url, MediaKind::Hls)).unwrap();
        assert_eq!(ticket.mime_type, "video/mp4");
        let client = local_client();
        let response = client.get(&ticket.url).send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Content-Type"], "video/mp4");
        assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
        assert_eq!(response.headers()["Content-Length"], "512");
        assert_eq!(response.bytes().unwrap().as_ref(), expected.as_slice());
        let response = client.get(&ticket.url).header("Range", "bytes=24-31").send().unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["Content-Type"], "video/mp4");
        assert_eq!(response.headers()["Content-Range"], "bytes 24-31/512");
        assert_eq!(response.headers()["Accept-Ranges"], "bytes");
        assert_eq!(response.bytes().unwrap().as_ref(), &expected[24..32]);
        let response = client.head(&ticket.url).send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Content-Type"], "video/mp4");
        assert_eq!(response.headers()["Content-Length"], "512");
        assert!(response.bytes().unwrap().is_empty());
        let unverified = proxy.register(source(&unverified_url, MediaKind::File)).unwrap();
        let response = client.get(&unverified.url).send().unwrap();
        assert_eq!(response.headers()["Content-Type"], "text/plain");
        assert_eq!(response.text().unwrap(), "unverified response");
        worker.join().unwrap();
    }

    #[test]
    fn rejects_unknown_tokens_foreign_origins_and_cancels_session() {
        let proxy = proxy();
        let ticket = proxy.register(source("https://example.invalid/movie.mp4", MediaKind::File)).unwrap();
        let client = local_client();
        let wrong = format!("http://{}/media/{}/0", proxy.inner.authority, "0".repeat(32));
        assert_eq!(client.get(wrong).send().unwrap().status(), 404);
        assert_eq!(client.get(&ticket.url).header("Origin", "https://example.invalid").send().unwrap().status(), 403);
        assert_eq!(client.get(&ticket.url).header("Range", "bytes=0-1,5-6").send().unwrap().status(), 416);
        let session = proxy.inner.sessions.lock().unwrap().get(&ticket.session_id).unwrap().clone();
        let mut reader = CancelableReader { inner: Cursor::new(vec![1; 256 * 1024]), canceled: Arc::clone(&session.canceled), cast_activity: None };
        let mut buffer = vec![0; 128 * 1024];
        assert_eq!(reader.read(&mut buffer).unwrap(), 64 * 1024);
        assert!(proxy.release(&ticket.session_id));
        assert_eq!(reader.read(&mut buffer).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(client.get(&ticket.url).send().unwrap().status(), 404);
    }

    #[test]
    fn redirect_does_not_forward_credentials_to_another_origin() {
        let first = Server::http("127.0.0.1:0").unwrap();
        let second = Server::http("127.0.0.1:0").unwrap();
        let initial = format!("http://{}/redirect", first.server_addr());
        let redirected = format!("http://{}/video.mp4", second.server_addr());
        let first_worker = thread::spawn(move || {
            let request = first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(header(&request, "Content-Auth"), Some("synthetic-private-token"));
            request.respond(Response::empty(302).with_header(Header::from_bytes("Location", redirected).unwrap())).unwrap();
        });
        let second_worker = thread::spawn(move || {
            let request = second.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(header(&request, "Content-Auth").is_none());
            request.respond(Response::from_data(vec![1,2,3])).unwrap();
        });
        let proxy = proxy();
        let ticket = proxy.register(source(&initial, MediaKind::File)).unwrap();
        assert_eq!(local_client().get(ticket.url).send().unwrap().bytes().unwrap().as_ref(), &[1,2,3]);
        first_worker.join().unwrap(); second_worker.join().unwrap();
    }

    fn cast_proxy_for_test() -> MediaProxy {
        // Exercise Cast HTTP rules over real TCP without selecting or activating
        // any LAN interface in the default offline test suite.
        MediaProxy::start_bound(Ipv4Addr::LOCALHOST, RelayAccess::Cast,
            |_, _| Ok((Client::builder().no_proxy(), Duration::from_secs(60)))).unwrap()
    }

    fn verified_fixture_ticket(proxy: &MediaProxy) -> PlaybackTicket {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/synthetic-container", upstream.server_addr());
        let worker = thread::spawn(move || {
            let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            request.respond(Response::from_data(mp4_prefix(512, 512))).unwrap();
        });
        let ticket = proxy.register_verified(source(&url, MediaKind::File)).unwrap();
        worker.join().unwrap();
        ticket
    }

    #[test]
    fn cast_bind_and_peer_policy_rejects_wildcards_public_and_other_address_families() {
        for address in ["0.0.0.0", "127.0.0.1", "169.254.1.1", "8.8.8.8", "203.0.113.1", "255.255.255.255"] {
            assert!(MediaProxy::start_cast(address.parse().unwrap(), |_, _| unreachable!()).is_err());
        }
        for address in ["10.1.2.3:123", "172.16.0.2:123", "172.31.255.1:123", "192.168.1.4:123", "127.0.0.1:123"] {
            assert!(RelayAccess::Cast.accepts_peer(address.parse().unwrap()));
        }
        for address in ["172.15.0.2:123", "172.32.0.2:123", "203.0.113.1:123", "169.254.1.1:123", "[::1]:123", "[fd00::1]:123"] {
            assert!(!RelayAccess::Cast.accepts_peer(address.parse().unwrap()));
        }
        assert!(!RelayAccess::Desktop.accepts_peer("192.168.1.4:123".parse().unwrap()));
    }

    fn exercise_cast_hls(cast: MediaProxy) {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_authority = upstream.server_addr().to_string();
        let url = format!("http://{upstream_authority}/hls/main?private=synthetic");
        const PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:3,\nsegment.ts\n#EXT-X-ENDLIST\n";
        let worker = thread::spawn(move || {
            for index in 0..5 {
                let request = upstream.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(header(&request, "Content-Auth"), Some("synthetic-private-token"));
                if index < 3 {
                    assert_eq!(request.url(), "/hls/main?private=synthetic");
                    assert_eq!(header(&request, "Range"), if index == 0 { Some("bytes=0-4095") } else { None });
                    request.respond(Response::from_string(PLAYLIST)
                        .with_header(Header::from_bytes("Content-Type", "application/vnd.apple.mpegurl").unwrap())).unwrap();
                } else if index == 3 {
                    assert_eq!(request.url(), "/hls/segment.ts");
                    assert_eq!(header(&request, "Range"), Some("bytes=2-5"));
                    request.respond(Response::from_data(b"2345".to_vec()).with_status_code(206)
                        .with_header(Header::from_bytes("Content-Range", "bytes 2-5/10").unwrap())
                        .with_header(Header::from_bytes("Content-Type", "video/mp2t").unwrap())).unwrap();
                } else {
                    assert_eq!(*request.method(), Method::Head);
                    request.respond(Response::from_data(b"0123456789".to_vec())
                        .with_header(Header::from_bytes("Content-Type", "video/mp2t").unwrap())).unwrap();
                }
            }
        });
        let local = proxy();
        let local_ticket = local.register_verified(source(&url, MediaKind::Hls)).unwrap();
        let client = local_client();
        assert_eq!(client.get(&local_ticket.url).header("Origin", "https://www.gstatic.com").send().unwrap().status(), 403);
        let local_playlist = client.get(&local_ticket.url).send().unwrap().text().unwrap();
        let original = local.inner.sessions.lock().unwrap()[&local_ticket.session_id].clone();
        let original_count = original.resources.lock().unwrap().entries.len();
        assert_eq!(original_count, 4);
        let ticket = local.export_session_into(&cast, &local_ticket).unwrap();
        assert_ne!(ticket.session_id, local_ticket.session_id);
        assert_eq!(ticket.mime_type, "application/vnd.apple.mpegurl");
        assert!(!ticket.url.contains("synthetic"));
        let exported = cast.inner.sessions.lock().unwrap()[&ticket.session_id].clone();
        assert_eq!(exported.resources.lock().unwrap().entries.len(), original_count);
        assert!(!Arc::ptr_eq(&original.canceled, &exported.canceled));

        let options = client.request(reqwest::Method::OPTIONS, &ticket.url)
            .header("Origin", "https://www.gstatic.com")
            .header("Access-Control-Request-Method", "GET")
            .header("Access-Control-Request-Headers", "range")
            .header("Access-Control-Request-Private-Network", "true").send().unwrap();
        assert_eq!(options.status(), 204);
        assert_eq!(options.headers()["Access-Control-Allow-Origin"], "*");
        assert_eq!(options.headers()["Access-Control-Allow-Private-Network"], "true");
        assert!(options.headers().get("Access-Control-Allow-Credentials").is_none());
        assert_eq!(client.get(&ticket.url).header("Host", local.inner.authority.as_str()).send().unwrap().status(), 403);
        assert_eq!(client.post(&ticket.url).send().unwrap().status(), 405);
        assert_eq!(client.get(&ticket.url).header("Range", "bytes=0-1,5-6").send().unwrap().status(), 416);
        let wrong_token = ticket.url.replace(&ticket.session_id, &"0".repeat(32));
        assert_eq!(client.get(wrong_token).send().unwrap().status(), 404);

        assert!(local.release(&local_ticket.session_id));
        assert!(original.canceled.load(Ordering::Acquire));
        assert!(!exported.canceled.load(Ordering::Acquire));
        assert_eq!(client.get(&local_ticket.url).send().unwrap().status(), 404);
        let response = client.get(&ticket.url).header("Origin", "https://www.gstatic.com")
            .header("Range", "bytes=0-4095").send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Access-Control-Allow-Origin"], "*");
        let playlist = response.text().unwrap();
        assert_eq!(playlist, local_playlist.replace(&local.inner.authority, &cast.inner.authority)
            .replace(&local_ticket.session_id, &ticket.session_id));
        assert!(!playlist.contains(&upstream_authority));
        assert!(!playlist.contains("synthetic"));
        let segment = playlist.lines().find(|line| line.starts_with("http://")).unwrap();
        let response = client.get(segment).header("Range", "bytes=2-5").send().unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["Content-Range"], "bytes 2-5/10");
        assert_eq!(response.bytes().unwrap().as_ref(), b"2345");
        let head = client.head(segment).send().unwrap();
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["Content-Length"], "10");
        assert!(head.bytes().unwrap().is_empty());
        register_resource(&cast.inner, &exported, &Url::parse(&url).unwrap(), "new-segment.ts", false).unwrap();
        assert_eq!(original.resources.lock().unwrap().entries.len(), original_count);
        assert_eq!(exported.resources.lock().unwrap().entries.len(), original_count + 1);
        assert!(cast.release(&ticket.session_id));
        assert_eq!(client.get(&ticket.url).send().unwrap().status(), 404);
        worker.join().unwrap();
    }

    #[test]
    fn cast_exports_verified_hls_with_independent_ticket_and_rewritten_resources() {
        exercise_cast_hls(cast_proxy_for_test());
    }

    #[test]
    fn cast_stop_and_drop_preserve_local_playback_and_credential_scope() {
        let first = Server::http("127.0.0.1:0").unwrap();
        let second = Server::http("127.0.0.1:0").unwrap();
        let initial = format!("http://{}/synthetic", first.server_addr());
        let destination = format!("http://{}/container", second.server_addr());
        let first_worker = thread::spawn(move || {
            for _ in 0..3 {
                let request = first.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(header(&request, "Content-Auth"), Some("synthetic-private-token"));
                request.respond(Response::empty(302).with_header(Header::from_bytes("Location", destination.as_str()).unwrap())).unwrap();
            }
        });
        let second_worker = thread::spawn(move || {
            for index in 0..3 {
                let request = second.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert!(header(&request, "Content-Auth").is_none());
                if index == 0 {
                    request.respond(Response::from_data(mp4_prefix(512, 512))).unwrap();
                } else {
                    assert_eq!(header(&request, "Range"), Some("bytes=2-5"));
                    request.respond(Response::from_data(b"2345".to_vec()).with_status_code(206)
                        .with_header(Header::from_bytes("Content-Range", "bytes 2-5/512").unwrap())).unwrap();
                }
            }
        });
        let local = proxy();
        let local_ticket = local.register_verified(source(&initial, MediaKind::File)).unwrap();
        let cast = cast_proxy_for_test();
        let ticket = local.export_session_into(&cast, &local_ticket).unwrap();
        let original = local.inner.sessions.lock().unwrap()[&local_ticket.session_id].clone();
        let exported = cast.inner.sessions.lock().unwrap()[&ticket.session_id].clone();
        let client = local_client();
        assert_eq!(client.get(&ticket.url).header("Range", "bytes=2-5").send().unwrap().bytes().unwrap().as_ref(), b"2345");
        let mut local_reader = CancelableReader { inner: Cursor::new([1; 10]), canceled: original.canceled.clone(), cast_activity: None };
        let mut cast_reader = CancelableReader { inner: Cursor::new([1; 10]), canceled: exported.canceled.clone(), cast_activity: Some(exported.clone()) };
        *exported.touched.lock().unwrap() = Instant::now() - SESSION_IDLE - Duration::from_secs(1);
        assert_eq!(cast_reader.read(&mut [0; 5]).unwrap(), 5);
        assert!(exported.touched.lock().unwrap().elapsed() < Duration::from_secs(1));
        assert!(cast.release(&ticket.session_id));
        assert_eq!(cast_reader.read(&mut [0; 10]).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(local_reader.read(&mut [0; 10]).unwrap(), 10);
        let another = local.export_session_into(&cast, &local_ticket).unwrap();
        let another_session = cast.inner.sessions.lock().unwrap()[&another.session_id].clone();
        drop(cast);
        assert!(another_session.canceled.load(Ordering::Acquire));
        assert!(!original.canceled.load(Ordering::Acquire));
        assert_eq!(client.get(&local_ticket.url).header("Range", "bytes=2-5").send().unwrap().bytes().unwrap().as_ref(), b"2345");
        first_worker.join().unwrap(); second_worker.join().unwrap();
    }

    #[test]
    fn cast_export_requires_verified_original_socket_and_enforces_limits_and_expiry() {
        let local = proxy();
        let cast = cast_proxy_for_test();
        let unverified = local.register(source("https://example.invalid/unverified", MediaKind::File)).unwrap();
        assert!(local.export_session_into(&cast, &unverified).is_err());
        assert!(cast.register(source("https://example.invalid/direct", MediaKind::File)).is_err());
        assert!(cast.register_verified(source("https://example.invalid/direct", MediaKind::File)).is_err());
        let ticket = verified_fixture_ticket(&local);
        let other_socket = proxy();
        let mut wrong = ticket.clone();
        wrong.url = wrong.url.replace(&local.inner.authority, &other_socket.inner.authority);
        assert!(local.export_session_into(&cast, &wrong).is_err());
        assert!(other_socket.export_session_into(&cast, &ticket).is_err());
        assert!(local.export_session_into(&other_socket, &ticket).is_err());
        let exported: Vec<_> = (0..MAX_SESSIONS).map(|_| local.export_session_into(&cast, &ticket).unwrap()).collect();
        assert!(local.export_session_into(&cast, &ticket).is_err());
        assert!(cast.export_session_into(&cast, &exported[0]).is_err());
        let stale = cast.inner.sessions.lock().unwrap()[&exported[0].session_id].clone();
        *stale.touched.lock().unwrap() = Instant::now() - SESSION_IDLE - Duration::from_secs(1);
        let client = local_client();
        assert_eq!(client.get(&exported[0].url).send().unwrap().status(), 410);
        assert!(stale.canceled.load(Ordering::Acquire));
        assert_eq!(client.get(&exported[0].url).send().unwrap().status(), 404);
        assert!(local.export_session_into(&cast, &ticket).is_ok());
        let original = local.inner.sessions.lock().unwrap()[&ticket.session_id].clone();
        *original.touched.lock().unwrap() = Instant::now() - SESSION_IDLE - Duration::from_secs(1);
        assert!(local.export_session_into(&cast, &ticket).is_err());
    }

    #[test]
    #[ignore = "Binds only the explicit XUPER_TEST_LAN_IPV4 local interface; no receiver or external service"]
    fn cast_explicit_private_interface_serves_only_its_selected_socket() {
        let ip: Ipv4Addr = std::env::var("XUPER_TEST_LAN_IPV4").expect("Set the local private interface explicitly").parse().unwrap();
        let cast = MediaProxy::start_cast(ip, |_, _| Ok((Client::builder().no_proxy(), Duration::from_secs(60)))).unwrap();
        let bound: SocketAddr = cast.inner.authority.parse().unwrap();
        assert_eq!(bound.ip(), ip);
        assert!(std::net::TcpStream::connect_timeout(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, bound.port())), Duration::from_millis(300)).is_err());
        exercise_cast_hls(cast);
    }
}
