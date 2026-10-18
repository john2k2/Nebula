//! Local Chrome sender. Only explicitly selected, verified media is exported
//! to the separate LAN relay; this listener exposes no generic application API.
use std::{collections::HashMap, io, net::{Ipv4Addr, UdpSocket}, path::PathBuf,
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}},
    thread, time::{Duration, Instant}};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};
use crate::media_proxy::{MediaProxy, PlaybackTicket};

const MAX_CONTROLLERS: usize = 4;
const CONTROL_LIFETIME: Duration = Duration::from_secs(6 * 60 * 60);
const WORKERS: usize = 4;
// Initialization must be serialized independently of the desktop/API workers.
// Keeping failures out of this slot also allows a later explicit action to retry.
static SENDER: Mutex<Option<Arc<Sender>>> = Mutex::new(None);

struct Selection {
    id: String, kind: String, title: String, start_time: f64, created: Instant,
    quality: Option<String>, audio_language: Option<String>,
    preparing: Mutex<()>, playback: Mutex<PlaybackState>,
}

pub struct PreparedMedia {
    pub ticket: PlaybackTicket,
    /// Only the resolved public quality/audio labels and bundle index.
    pub selection: Value,
}
#[derive(Default)]
struct PlaybackState {
    generation: u64, closed: bool, ticket: Option<PlaybackTicket>,
}
struct State {
    server: Arc<Server>, authority: String,
    selections: Mutex<HashMap<String, Arc<Selection>>>,
    media: Arc<dyn CastMedia>, stopped: AtomicBool, active_preparations: AtomicUsize,
}
struct Sender { state: Arc<State>, workers: Vec<thread::JoinHandle<()>> }

// This narrow capability cannot select an arbitrary network URL or dispatch
// application commands. Tests replace it without starting a relay or API call.
trait CastMedia: Send + Sync {
    fn prepare(&self, kind: &str, id: &str, quality: Option<&str>, audio_language: Option<&str>) -> Result<PreparedMedia, String>;
    fn release(&self, session_id: &str);
}
#[derive(Default)]
struct RelayMedia { relay: Mutex<Option<Arc<MediaProxy>>> }

impl CastMedia for RelayMedia {
    fn prepare(&self, kind: &str, id: &str, quality: Option<&str>, audio_language: Option<&str>) -> Result<PreparedMedia, String> {
        let relay = {
            let mut value = self.relay.lock().map_err(|_| "No se pudo preparar el envío al televisor.")?;
            if value.is_none() {
                *value = Some(Arc::new(MediaProxy::start_cast(local_address()?, crate::protocol::media_client_builder_until)?));
            }
            Arc::clone(value.as_ref().unwrap())
        };
        crate::api::cast_playback(kind, id, quality, audio_language, &relay)
    }

    fn release(&self, session_id: &str) {
        let relay = self.relay.lock().ok().and_then(|relay| relay.clone());
        if let Some(relay) = relay { relay.release(session_id); }
    }
}

impl Selection {
    fn from_params(params: &Value) -> Result<Self, String> {
        let id = params.get("id").and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= 256)
            .ok_or("Selecciona un título para enviarlo al televisor.")?;
        let kind = params.get("kind").and_then(Value::as_str).filter(|v| matches!(*v, "vod" | "live"))
            .ok_or("Selecciona un contenido compatible.")?;
        let title = params.get("title").and_then(Value::as_str).unwrap_or("Nébula");
        if title.len() > 2000 { return Err("El nombre del título es demasiado largo.".into()); }
        let start_time = params.get("start_time").and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 86_400.0).unwrap_or(0.0);
        // Validate before opening Chrome. Availability is validated again by
        // playback against the selected title's actual authorized variants.
        let preferences = crate::api::cast_playback_params(kind, id,
            optional_preference(params, "quality")?, optional_preference(params, "audio_language")?)?;
        let quality = preferences.get("quality").and_then(Value::as_str).map(str::to_owned);
        let audio_language = preferences.get("audio_language").and_then(Value::as_str).map(str::to_owned);
        Ok(Self { id: id.into(), kind: kind.into(), title: title.into(), start_time, quality, audio_language,
            created: Instant::now(), preparing: Mutex::new(()), playback: Mutex::new(PlaybackState::default()) })
    }
}

fn optional_preference<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        _ => Err("La selección de calidad o idioma no es válida.".into()),
    }
}

fn initialized_sender(slot: &Mutex<Option<Arc<Sender>>>, start: impl FnOnce() -> Result<Sender, String>) -> Result<Arc<Sender>, String> {
    let mut value = slot.lock().map_err(|_| "No se pudo abrir el control del televisor.")?;
    if let Some(sender) = value.as_ref() { return Ok(Arc::clone(sender)); }
    let sender = Arc::new(start()?);
    *value = Some(Arc::clone(&sender));
    Ok(sender)
}

impl Sender {
    fn start() -> Result<Self, String> {
        Self::start_with_media(Arc::new(RelayMedia::default()))
    }

    fn start_with_media(media: Arc<dyn CastMedia>) -> Result<Self, String> {
        Self::start_with_spawner(media, |index, work| thread::Builder::new().name(format!("xuper-cast-{index}")).spawn(work))
    }

    fn start_with_spawner<F>(media: Arc<dyn CastMedia>, mut spawn: F) -> Result<Self, String>
    where F: FnMut(usize, Box<dyn FnOnce() + Send>) -> io::Result<thread::JoinHandle<()>> {
        let server = Arc::new(Server::http("127.0.0.1:0").map_err(|_| "No se pudo abrir el control local del televisor.")?);
        let authority = server.server_addr().to_ip().ok_or("No hay dirección local para Chromecast.")?.to_string();
        let state = Arc::new(State { server, authority, selections: Mutex::new(HashMap::new()), media,
            stopped: AtomicBool::new(false), active_preparations: AtomicUsize::new(0) });
        // Construct the owner before spawning so any partial failure runs Drop.
        let mut sender = Self { state, workers: Vec::with_capacity(WORKERS) };
        for index in 0..WORKERS {
            let worker = Arc::clone(&sender.state);
            let handle = spawn(index, Box::new(move || {
                while !worker.stopped.load(Ordering::Acquire) {
                    match worker.server.recv_timeout(Duration::from_millis(250)) {
                        Ok(Some(request)) if !worker.stopped.load(Ordering::Acquire) => serve(&worker, request),
                        Ok(Some(request)) => reply(request, 503, "text/plain", b"Control cerrado."),
                        Ok(None) => {},
                        Err(_) => break,
                    }
                }
            })).map_err(|_| "No se pudo iniciar el control del televisor.")?;
            sender.workers.push(handle);
        }
        Ok(sender)
    }

    fn add_selection(&self, selection: Selection) -> Result<String, String> {
        let expired = {
            let mut values = self.state.selections.lock().map_err(|_| "No se pudo preparar el envío.")?;
            let keys: Vec<_> = values.iter().filter(|(_, value)| value.created.elapsed() >= CONTROL_LIFETIME)
                .map(|(key, _)| key.clone()).collect();
            keys.into_iter().filter_map(|key| values.remove(&key)).collect::<Vec<_>>()
        };
        for selection in expired { stop(&self.state, &selection, true); }
        let mut values = self.state.selections.lock().map_err(|_| "No se pudo preparar el envío.")?;
        if values.len() >= MAX_CONTROLLERS {
            return Err("Cierra un control anterior con «Cerrar control» antes de abrir otro.".into());
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        values.insert(token.clone(), Arc::new(selection));
        Ok(token)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        let selections = self.state.selections.lock().map(|mut values| std::mem::take(&mut *values)).unwrap_or_default();
        for selection in selections.values() { stop(&self.state, selection, true); }
        for _ in 0..self.workers.len() { self.state.server.unblock(); }
        // Idle workers exit immediately; an in-flight bounded API request can
        // finish without blocking shutdown and must release its late ticket.
        for worker in self.workers.drain(..) { if worker.is_finished() { let _ = worker.join(); } }
    }
}

fn local_address() -> Result<Ipv4Addr, String> {
    // UDP connect chooses a route without transmitting a packet. No DNS or
    // discovery scan is performed; Chrome owns Chromecast discovery/selection.
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|_| "No se pudo detectar la red del equipo.")?;
    socket.connect("8.8.8.8:53").map_err(|_| "Conecta esta PC y el televisor a la misma red.")?;
    match socket.local_addr().map(|address| address.ip()) {
        Ok(std::net::IpAddr::V4(address)) if address.is_private() => Ok(address),
        _ => Err("No se encontró una red local para enviar el video al televisor.".into()),
    }
}

fn stop(state: &State, selection: &Selection, close: bool) {
    let ticket = selection.playback.lock().ok().and_then(|mut playback| {
        playback.generation = playback.generation.wrapping_add(1);
        playback.closed |= close;
        playback.ticket.take()
    });
    if let Some(ticket) = ticket { state.media.release(&ticket.session_id); }
}

fn close_selection(state: &State, token: &str) {
    let selection = state.selections.lock().ok().and_then(|mut values| values.remove(token));
    if let Some(selection) = selection { stop(state, &selection, true); }
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers().iter().find(|value| value.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|value| value.value.as_str())
}
fn reply(request: Request, status: u16, mime: &str, body: impl Into<Vec<u8>>) {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [("Content-Type", mime), ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"), ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", "frame-ancestors 'none'; base-uri 'none'; object-src 'none'")] {
        response.add_header(Header::from_bytes(name, value).unwrap());
    }
    let _ = request.respond(response);
}
fn json_reply(request: Request, status: u16, value: Value) {
    reply(request, status, "application/json; charset=utf-8", serde_json::to_vec(&value).unwrap());
}

struct PreparationSlot<'a>(&'a AtomicUsize);
impl<'a> PreparationSlot<'a> {
    fn acquire(state: &'a State) -> Option<Self> {
        // Leave a worker available for stop/close even when every controller
        // tries to prepare at once. The API itself may take up to its deadline.
        state.active_preparations.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |active| (active < WORKERS - 1).then_some(active + 1)).ok()?;
        Some(Self(&state.active_preparations))
    }
}
impl Drop for PreparationSlot<'_> {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); }
}

fn prepare(state: &State, selection: &Selection, request: Request) {
    let Ok(_preparing) = selection.preparing.try_lock() else {
        json_reply(request, 409, json!({"error":"Espera a que termine la solicitud anterior."})); return;
    };
    let Some(_slot) = PreparationSlot::acquire(state) else {
        json_reply(request, 409, json!({"error":"Hay otros videos preparándose. Inténtalo en unos segundos."})); return;
    };
    let begun = selection.playback.lock().ok().and_then(|mut playback| {
        if playback.closed || state.stopped.load(Ordering::Acquire) { return None; }
        playback.generation = playback.generation.wrapping_add(1);
        Some((playback.generation, playback.ticket.take()))
    });
    let Some((generation, previous)) = begun else {
        json_reply(request, 410, json!({"error":"Este control ya está cerrado."})); return;
    };
    if let Some(ticket) = previous { state.media.release(&ticket.session_id); }
    match state.media.prepare(&selection.kind, &selection.id, selection.quality.as_deref(), selection.audio_language.as_deref()) {
        Ok(PreparedMedia { ticket, selection: resolved }) => {
            let value = json!({"url":ticket.url,"mime_type":ticket.mime_type,"title":selection.title,
                "kind":selection.kind,"start_time":selection.start_time,"selection":resolved});
            let mut pending = Some(ticket);
            let accepted = selection.playback.lock().map(|mut playback| {
                if playback.closed || playback.generation != generation || state.stopped.load(Ordering::Acquire)
                    || selection.created.elapsed() >= CONTROL_LIFETIME { return false; }
                playback.ticket = pending.take();
                true
            }).unwrap_or(false);
            if accepted { json_reply(request, 200, value); }
            else {
                if let Some(ticket) = pending { state.media.release(&ticket.session_id); }
                json_reply(request, 409, json!({"error":"El envío fue cancelado."}));
            }
        }
        Err(error) => {
            let canceled = state.stopped.load(Ordering::Acquire) || selection.playback.lock()
                .map(|playback| playback.closed || playback.generation != generation).unwrap_or(true);
            if canceled { json_reply(request, 409, json!({"error":"El envío fue cancelado."})); }
            else { json_reply(request, 502, json!({"error":error})); }
        }
    }
}

fn serve(state: &Arc<State>, request: Request) {
    let origin = format!("http://{}", state.authority);
    if request.remote_addr().is_none_or(|address| !address.ip().is_loopback())
        || header(&request, "Host") != Some(state.authority.as_str())
        || header(&request, "Origin").is_some_and(|value| value != origin) {
        reply(request, 403, "text/plain", b"Solicitud no permitida."); return;
    }
    let route: Vec<_> = request.url().split('/').collect();
    if route.len() != 4 || route[1] != "cast" || route[2].len() != 32 || !route[2].bytes().all(|b| b.is_ascii_hexdigit()) {
        reply(request, 404, "text/plain", b"Control no disponible."); return;
    }
    let token = route[2].to_string();
    let action = route[3].to_string();
    let selection = state.selections.lock().ok().and_then(|values| values.get(&token).cloned());
    let Some(selection) = selection else {
        reply(request, 410, "text/plain", "Este control ha caducado. Abre Chromecast desde Nébula.".as_bytes()); return;
    };
    if selection.created.elapsed() >= CONTROL_LIFETIME {
        close_selection(state, &token);
        reply(request, 410, "text/plain", "Este control ha caducado. Abre Chromecast desde Nébula.".as_bytes()); return;
    }
    if *request.method() == Method::Get {
        match action.as_str() {
            "" => reply(request, 200, "text/html; charset=utf-8", include_bytes!("../frontend/cast/index.html").to_vec()),
            "app.js" => reply(request, 200, "text/javascript; charset=utf-8", include_bytes!("../frontend/cast/app.js").to_vec()),
            "app.css" => reply(request, 200, "text/css; charset=utf-8", include_bytes!("../frontend/cast/app.css").to_vec()),
            "state" => json_reply(request, 200, json!({"title":selection.title,"kind":selection.kind,"start_time":selection.start_time,
                "preferences":{"quality":selection.quality,"audio_language":selection.audio_language}})),
            _ => reply(request, 404, "text/plain", b"Recurso no disponible."),
        }
        return;
    }
    if *request.method() != Method::Post || header(&request, "Origin") != Some(origin.as_str())
        || header(&request, "Content-Type") != Some("application/json")
        || request.body_length().is_none_or(|length| length > 2) {
        reply(request, 403, "text/plain", b"Solicitud no permitida."); return;
    }
    match action.as_str() {
        "prepare" => prepare(state, &selection, request),
        "stop" | "close" => {
            if action == "close" { close_selection(state, &token); }
            else { stop(state, &selection, false); }
            json_reply(request, 200, json!({"stopped":true}));
        }
        _ => reply(request, 404, "text/plain", b"Recurso no disponible."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, net::SocketAddr, sync::{Barrier, Condvar, mpsc}};
    use reqwest::blocking::{Client, Response as HttpResponse};

    struct FakeMedia {
        calls: Mutex<Vec<(String, String, Option<String>, Option<String>)>>,
        active: Mutex<HashSet<String>>,
        released: Mutex<Vec<String>>,
        next: AtomicUsize,
        blocked: Mutex<bool>,
        wake: Condvar,
        started: mpsc::Sender<String>,
    }
    impl FakeMedia {
        fn new(blocked: bool) -> (Arc<Self>, mpsc::Receiver<String>) {
            let (started, receive) = mpsc::channel();
            (Arc::new(Self { calls: Mutex::new(Vec::new()), active: Mutex::new(HashSet::new()),
                released: Mutex::new(Vec::new()), next: AtomicUsize::new(1), blocked: Mutex::new(blocked), wake: Condvar::new(), started }), receive)
        }
        fn unblock(&self) {
            *self.blocked.lock().unwrap() = false;
            self.wake.notify_all();
        }
    }
    impl CastMedia for FakeMedia {
        fn prepare(&self, kind: &str, id: &str, quality: Option<&str>, audio_language: Option<&str>) -> Result<PreparedMedia, String> {
            self.calls.lock().unwrap().push((kind.into(), id.into(), quality.map(str::to_owned), audio_language.map(str::to_owned)));
            let _ = self.started.send(id.into());
            let (blocked, _) = self.wake.wait_timeout_while(self.blocked.lock().unwrap(), Duration::from_secs(3), |blocked| *blocked).unwrap();
            if *blocked { return Err("Synthetic fixture timed out".into()); }
            let session_id = format!("{:032x}", self.next.fetch_add(1, Ordering::AcqRel));
            self.active.lock().unwrap().insert(session_id.clone());
            // This is metadata only. No test requests this synthetic media URL.
            let selection = if quality.is_some() || audio_language.is_some() {
                json!({"quality":quality.unwrap_or("480p"),"audio_language":audio_language,"audio_track_index":audio_language.map(|_|1)})
            } else { Value::Null };
            Ok(PreparedMedia { ticket: PlaybackTicket { url: format!("http://127.0.0.1:1/media/{session_id}/0"), mime_type: "video/mp4".into(), session_id }, selection })
        }
        fn release(&self, session_id: &str) {
            self.active.lock().unwrap().remove(session_id);
            self.released.lock().unwrap().push(session_id.into());
        }
    }

    fn client() -> Client { Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap() }
    fn selection(id: &str) -> Selection {
        Selection::from_params(&json!({"id":id,"kind":"vod","title":"Título sintético","start_time":12.5})).unwrap()
    }
    fn control(sender: &Sender, token: &str) -> String { format!("http://{}/cast/{token}/", sender.state.authority) }
    fn post(client: &Client, url: &str, action: &str) -> HttpResponse {
        let origin = reqwest::Url::parse(url).unwrap().origin().ascii_serialization();
        client.post(format!("{url}{action}")).header("Origin", origin).header("Content-Type", "application/json").body("{}").send().unwrap()
    }

    #[cfg(windows)]
    fn controller_listener_present(address: SocketAddr) -> io::Result<bool> {
        // Read the kernel's IPv4 LISTEN table, filtered to this process and exact
        // endpoint. Unlike a short connect timeout, this distinguishes an open
        // listener from Windows' delayed connection errors and TIME_WAIT rows.
        // Win32 ABI: TCP_TABLE_OWNER_PID_LISTENER = 3, AF_INET = 2; each
        // MIB_TCPROW_OWNER_PID contains six DWORDs after the entry-count DWORD.
        #[link(name = "iphlpapi")]
        unsafe extern "system" {
            #[link_name = "GetExtendedTcpTable"]
            fn tcp_table(table: *mut std::ffi::c_void, size: *mut u32, sorted: i32, family: u32, class: i32, reserved: u32) -> u32;
        }
        let SocketAddr::V4(address) = address else { return Err(io::Error::other("The controller fixture requires IPv4")); };
        const NEED_BUFFER: u32 = 122;
        let mut size = 0;
        let status = unsafe { tcp_table(std::ptr::null_mut(), &mut size, 0, 2, 3, 0) };
        if status != 0 && status != NEED_BUFFER { return Err(io::Error::from_raw_os_error(status as i32)); }
        for _ in 0..4 {
            if !(4..=1024 * 1024).contains(&size) { return Err(io::Error::other("Unexpected TCP table size")); }
            // DWORD storage gives the returned C structures their required
            // alignment. The size can grow concurrently, so retry that case.
            let mut table = vec![0u32; (size as usize).div_ceil(4)];
            let status = unsafe { tcp_table(table.as_mut_ptr().cast(), &mut size, 0, 2, 3, 0) };
            if status == NEED_BUFFER { continue; }
            if status != 0 { return Err(io::Error::from_raw_os_error(status as i32)); }
            let count = table[0] as usize;
            if count > (table.len() - 1) / 6 { return Err(io::Error::other("Incomplete TCP table")); }
            return Ok(table[1..1 + count * 6].chunks_exact(6).any(|row| {
                row[0] == 2 && row[1].to_ne_bytes() == address.ip().octets()
                    && u16::from_be(row[2] as u16) == address.port() && row[5] == std::process::id()
            }));
        }
        Err(io::Error::other("TCP table kept changing during shutdown check"))
    }

    #[test]
    fn singleton_initialization_is_serialized_and_failure_can_retry() {
        let slot = Arc::new(Mutex::new(None));
        assert!(initialized_sender(&slot, || Err("Synthetic init failure".into())).is_err());
        assert!(slot.lock().unwrap().is_none());
        let (media, _) = FakeMedia::new(false);
        let count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(4));
        let workers: Vec<_> = (0..4).map(|_| {
            let slot = Arc::clone(&slot); let count = Arc::clone(&count); let barrier = Arc::clone(&barrier); let media = Arc::clone(&media);
            thread::spawn(move || {
                barrier.wait();
                initialized_sender(&slot, || {
                    count.fetch_add(1, Ordering::AcqRel);
                    Sender::start_with_media(media)
                }).unwrap()
            })
        }).collect();
        let senders: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(count.load(Ordering::Acquire), 1);
        assert!(senders.iter().all(|sender| Arc::ptr_eq(sender, &senders[0])));
        let weak = Arc::downgrade(&senders[0].state);
        slot.lock().unwrap().take();
        drop(senders);
        let deadline = Instant::now() + Duration::from_secs(1);
        while weak.upgrade().is_some() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
        assert!(weak.upgrade().is_none(), "Idle sender workers must release their server");
    }

    #[test]
    fn partial_worker_spawn_failure_stops_already_started_workers() {
        let (media, _) = FakeMedia::new(false);
        let (finished, receive) = mpsc::channel();
        let result = Sender::start_with_spawner(media, move |index, work| {
            if index == 2 { return Err(io::Error::other("Synthetic spawn failure")); }
            let finished = finished.clone();
            thread::Builder::new().spawn(move || { work(); let _ = finished.send(index); })
        });
        assert!(result.is_err());
        // Observe both workers actually exiting, with the same scheduling
        // allowance as the other local socket fixtures on Windows.
        let mut exited = vec![receive.recv_timeout(Duration::from_secs(5)).unwrap(), receive.recv_timeout(Duration::from_secs(5)).unwrap()];
        exited.sort_unstable();
        assert_eq!(exited, [0, 1]);
    }

    #[test]
    fn controller_http_surface_only_prepares_the_selected_item_and_releases_its_ticket() {
        let (media, _) = FakeMedia::new(false);
        let sender = Sender::start_with_media(media.clone()).unwrap();
        let first = sender.add_selection(selection("selected-one")).unwrap();
        let second = sender.add_selection(selection("selected-two")).unwrap();
        let (first_url, second_url) = (control(&sender, &first), control(&sender, &second));
        let client = client();
        let state = client.get(format!("{first_url}state")).send().unwrap();
        assert_eq!(state.status(), 200);
        assert_eq!(state.headers()["Cache-Control"], "no-store");
        let state: Value = state.json().unwrap();
        assert_eq!(state, json!({"kind":"vod","title":"Título sintético","start_time":12.5,
            "preferences":{"quality":null,"audio_language":null}}));
        for file in ["", "app.js", "app.css"] { assert_eq!(client.get(format!("{first_url}{file}")).send().unwrap().status(), 200); }
        assert_eq!(client.get(format!("{first_url}prepare")).send().unwrap().status(), 404);
        assert_eq!(client.get(format!("{first_url}state")).header("Host", "other.invalid").send().unwrap().status(), 403);
        assert_eq!(client.get(format!("{first_url}state")).header("Origin", "https://other.invalid").send().unwrap().status(), 403);
        assert_eq!(client.post(format!("{first_url}prepare")).header("Content-Type", "application/json").body("{}").send().unwrap().status(), 403);
        assert_eq!(client.post(format!("{first_url}prepare")).header("Origin", format!("http://{}", sender.state.authority))
            .header("Content-Type", "application/json").body(r#"{"id":"different-item","url":"https://example.invalid"}"#).send().unwrap().status(), 403);
        assert_eq!(post(&client, &first_url, "api").status(), 404);
        assert_eq!(post(&client, &first_url, "prepare?url=https://example.invalid").status(), 404);
        assert!(media.calls.lock().unwrap().is_empty());
        let first_response = post(&client, &first_url, "prepare");
        assert_eq!(first_response.status(), 200);
        let first_ticket: Value = first_response.json().unwrap();
        assert_eq!(post(&client, &second_url, "prepare").status(), 200);
        assert_eq!(media.calls.lock().unwrap().as_slice(), &[("vod".into(), "selected-one".into(), None, None), ("vod".into(), "selected-two".into(), None, None)]);
        assert_eq!(media.active.lock().unwrap().len(), 2);
        assert_eq!(post(&client, &first_url, "stop").status(), 200);
        assert_eq!(media.active.lock().unwrap().len(), 1);
        assert!(first_ticket["url"].as_str().unwrap().contains(&media.released.lock().unwrap()[0]));
        assert_eq!(client.get(format!("{first_url}state")).send().unwrap().status(), 200);
        assert_eq!(post(&client, &second_url, "close").status(), 200);
        assert!(media.active.lock().unwrap().is_empty());
        assert_eq!(client.get(format!("{second_url}state")).send().unwrap().status(), 410);
    }

    #[test]
    fn controller_preserves_selected_quality_and_language_without_accepting_browser_overrides() {
        let (media, _) = FakeMedia::new(false);
        let sender = Sender::start_with_media(media.clone()).unwrap();
        let mut params = json!({"id":"selected-episode","kind":"vod","title":"Título sintético",
            "quality":"720p","audio_language":"spa","audio_track_index":999});
        let token = sender.add_selection(Selection::from_params(&params).unwrap()).unwrap();
        params["quality"] = json!("1080p");
        params["audio_language"] = json!("eng");
        let url = control(&sender, &token);
        let client = client();
        let state: Value = client.get(format!("{url}state")).send().unwrap().json().unwrap();
        assert_eq!(state["preferences"], json!({"quality":"720p","audio_language":"spa"}));
        assert!(state["preferences"].get("audio_track_index").is_none());
        let origin = format!("http://{}", sender.state.authority);
        let rejected = client.post(format!("{url}prepare")).header("Origin", origin)
            .header("Content-Type", "application/json").body(r#"{"quality":"1080p","audio_language":"eng"}"#).send().unwrap();
        assert_eq!(rejected.status(), 403);
        assert!(media.calls.lock().unwrap().is_empty());
        let response = post(&client, &url, "prepare");
        assert_eq!(response.status(), 200);
        let response: Value = response.json().unwrap();
        assert_eq!(response["selection"], json!({"quality":"720p","audio_language":"spa","audio_track_index":1}));
        assert_eq!(media.calls.lock().unwrap().as_slice(), &[("vod".into(), "selected-episode".into(), Some("720p".into()), Some("spa".into()))]);
        assert_eq!(post(&client, &url, "close").status(), 200);
        assert!(media.active.lock().unwrap().is_empty());
    }

    #[test]
    fn cast_preferences_use_backend_validation_before_initializing_a_sender() {
        let base = json!({"id":"selected","kind":"vod"});
        for invalid in [json!({"quality":"4k"}),json!({"quality":720}),json!({"quality":""}),
            json!({"audio_language":"por,eng"}),json!({"audio_language":1}),json!({"audio_language":"https://example.invalid"})] {
            let mut params = base.clone();
            for (name, value) in invalid.as_object().unwrap() { params[name] = value.clone(); }
            assert!(Selection::from_params(&params).is_err());
        }
        assert!(Selection::from_params(&json!({"id":"selected","kind":"live","quality":"720p"})).is_err());
        let params = crate::api::cast_playback_params("vod", "selected", Some("1080p"), Some("pt-BR")).unwrap();
        assert_eq!(params, json!({"kind":"vod","id":"selected","quality":"1080p","audio_language":"pt-BR"}));
        assert!(crate::api::cast_playback_params("vod", "selected", Some("4k"), None).is_err());
        assert!(crate::api::cast_playback_params("vod", "selected", None, Some("private-token")).is_err());
        let defaults = Selection::from_params(&base).unwrap();
        assert!(defaults.quality.is_none() && defaults.audio_language.is_none());
    }

    #[test]
    fn stop_and_close_remain_available_during_preparation_and_release_late_tickets() {
        let (media, started) = FakeMedia::new(true);
        let sender = Sender::start_with_media(media.clone()).unwrap();
        let urls: Vec<_> = (0..MAX_CONTROLLERS).map(|index| {
            let token = sender.add_selection(selection(&format!("selected-{index}"))).unwrap();
            control(&sender, &token)
        }).collect();
        let requests: Vec<_> = urls[..WORKERS - 1].iter().map(|url| {
            let url = url.clone(); thread::spawn(move || post(&client(), &url, "prepare").status().as_u16())
        }).collect();
        for _ in 0..WORKERS - 1 { started.recv_timeout(Duration::from_secs(1)).unwrap(); }
        let client = client();
        let before = Instant::now();
        assert_eq!(post(&client, &urls[3], "prepare").status(), 409);
        assert_eq!(post(&client, &urls[0], "prepare").status(), 409);
        assert_eq!(post(&client, &urls[0], "stop").status(), 200);
        assert_eq!(post(&client, &urls[1], "close").status(), 200);
        assert!(before.elapsed() < Duration::from_secs(1), "Control requests must not wait for preparation");
        media.unblock();
        let statuses: Vec<_> = requests.into_iter().map(|request| request.join().unwrap()).collect();
        assert_eq!(statuses, [409, 409, 200]);
        assert_eq!(media.released.lock().unwrap().len(), 2);
        assert_eq!(media.active.lock().unwrap().len(), 1);
        assert_eq!(sender.state.active_preparations.load(Ordering::Acquire), 0);
        assert_eq!(post(&client, &urls[2], "close").status(), 200);
        assert!(media.active.lock().unwrap().is_empty());
    }

    #[test]
    fn expired_controllers_release_media_and_controller_limit_recovers_after_close() {
        let (media, _) = FakeMedia::new(false);
        let sender = Sender::start_with_media(media.clone()).unwrap();
        let mut expired = selection("expired");
        expired.created = Instant::now() - CONTROL_LIFETIME - Duration::from_secs(1);
        expired.playback.lock().unwrap().ticket = Some(media.prepare("vod", "expired", None, None).unwrap().ticket);
        let token = sender.add_selection(expired).unwrap();
        assert_eq!(client().get(format!("{}state", control(&sender, &token))).send().unwrap().status(), 410);
        assert!(media.active.lock().unwrap().is_empty());
        assert!(sender.state.selections.lock().unwrap().is_empty());
        let tokens: Vec<_> = (0..MAX_CONTROLLERS).map(|index| sender.add_selection(selection(&index.to_string())).unwrap()).collect();
        assert!(sender.add_selection(selection("over-limit")).is_err());
        close_selection(&sender.state, &tokens[0]);
        assert!(sender.add_selection(selection("replacement")).is_ok());
        assert!(Selection::from_params(&json!({"id":"","kind":"vod"})).is_err());
        assert!(Selection::from_params(&json!({"id":"selected","kind":"arbitrary-command"})).is_err());
    }

    #[test]
    fn dropping_sender_releases_sessions_and_closes_its_listener() {
        let (media, _) = FakeMedia::new(false);
        let sender = Sender::start_with_media(media.clone()).unwrap();
        let token = sender.add_selection(selection("selected")).unwrap();
        assert_eq!(post(&client(), &control(&sender, &token), "prepare").status(), 200);
        let address: SocketAddr = sender.state.authority.parse().unwrap();
        #[cfg(windows)]
        assert!(controller_listener_present(address).unwrap(), "The observer must detect the fixture's live listener");
        let weak = Arc::downgrade(&sender.state);
        drop(sender);
        assert!(media.active.lock().unwrap().is_empty());
        let deadline = Instant::now() + Duration::from_secs(1);
        while weak.upgrade().is_some() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
        assert!(weak.upgrade().is_none());
        // tiny_http 0.12 owns the listener in a separate accept thread. Its
        // Server::drop sets a close flag and wakes accept with a self-connect;
        // it does not join that thread. State/worker destruction can therefore
        // precede the actual socket close. Require the kernel's LISTEN entry to
        // disappear within one second, rather than inferring closure from TCP
        // connection timing. Query failures remain test failures.
        #[cfg(windows)]
        {
            let socket_deadline = Instant::now() + Duration::from_secs(1);
            while controller_listener_present(address).unwrap() {
                assert!(Instant::now() < socket_deadline, "The controller listener remained open after shutdown");
                thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(not(windows))]
        assert!(std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }
}

