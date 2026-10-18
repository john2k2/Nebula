//! LibVLC stays in-process. Only verified relay sessions reach this module.
//! Frames cross into our WebView in a bounded shared buffer, never JSON/base64.
use std::{ffi::{c_char, c_int, c_void, CStr, CString}, path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock}, os::windows::ffi::OsStrExt};
use serde_json::{json, Value};
use windows::{core::{Interface, PCSTR, PCWSTR}, Win32::System::LibraryLoader::{LoadLibraryExW, GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS}};
use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Environment12, ICoreWebView2_17,
    ICoreWebView2SharedBuffer, COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY};
use wry::WebViewExtWindows;

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
pub const BUFFER_BYTES: usize = MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;
type Ptr = *mut c_void;
type Lock = unsafe extern "C" fn(Ptr, *mut Ptr) -> Ptr;
type Unlock = unsafe extern "C" fn(Ptr, Ptr, *const Ptr);
type Display = unsafe extern "C" fn(Ptr, Ptr);
type Format = unsafe extern "C" fn(*mut Ptr, *mut c_char, *mut u32, *mut u32, *mut u32, *mut u32) -> u32;
type Cleanup = unsafe extern "C" fn(Ptr);
#[repr(C)] pub(crate) struct Description { pub(crate) id: c_int, pub(crate) name: *mut c_char, pub(crate) next: *mut Description }
#[repr(C)] struct MediaTrack { codec: u32, original: u32, id: c_int, kind: c_int, profile: c_int,
    level: c_int, data: Ptr, bitrate: u32, language: *mut c_char, description: *mut c_char }
#[repr(C)] struct MediaStats { values: [u32; 15] }

macro_rules! library {
    ($($field:ident: $name:literal => $ty:ty),+ $(,)?) => {
        pub(crate) struct Library { $(pub(crate) $field: $ty),+ }
        impl Library {
            pub(crate) unsafe fn load(path: &std::path::Path) -> Result<Self, String> {
                let canonical=std::fs::canonicalize(path).map_err(|_|"No se encontró la biblioteca VLC del paquete.")?;
                let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
                let module = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }
                    .map_err(|error| format!("No se pudo cargar el motor VLC incluido en la aplicación ({:08X}).",error.code().0))?;
                // The DLL remains loaded for process lifetime. Callback pointers cannot dangle.
                Ok(Self { $($field: unsafe {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(
                        GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr()))
                            .ok_or(concat!("El motor VLC no contiene ", $name))?)
                }),+ })
            }
        }
    }
}
library! {
    instance_release: "libvlc_release" => unsafe extern "C" fn(Ptr),
    renderer_new: "libvlc_renderer_discoverer_new" => unsafe extern "C" fn(Ptr,*const c_char)->Ptr,
    renderer_start: "libvlc_renderer_discoverer_start" => unsafe extern "C" fn(Ptr)->c_int,
    renderer_stop: "libvlc_renderer_discoverer_stop" => unsafe extern "C" fn(Ptr),
    renderer_release: "libvlc_renderer_discoverer_release" => unsafe extern "C" fn(Ptr),
    renderer_events: "libvlc_renderer_discoverer_event_manager" => unsafe extern "C" fn(Ptr)->Ptr,
    event_attach: "libvlc_event_attach" => unsafe extern "C" fn(Ptr,c_int,unsafe extern "C" fn(*const crate::native_cast::RendererEvent,Ptr),Ptr)->c_int,
    event_detach: "libvlc_event_detach" => unsafe extern "C" fn(Ptr,c_int,unsafe extern "C" fn(*const crate::native_cast::RendererEvent,Ptr),Ptr),
    renderer_hold: "libvlc_renderer_item_hold" => unsafe extern "C" fn(Ptr)->Ptr,
    renderer_item_release: "libvlc_renderer_item_release" => unsafe extern "C" fn(Ptr),
    renderer_name: "libvlc_renderer_item_name" => unsafe extern "C" fn(Ptr)->*const c_char,
    renderer_type: "libvlc_renderer_item_type" => unsafe extern "C" fn(Ptr)->*const c_char,
    renderer_flags: "libvlc_renderer_item_flags" => unsafe extern "C" fn(Ptr)->c_int,
    set_renderer: "libvlc_media_player_set_renderer" => unsafe extern "C" fn(Ptr,Ptr)->c_int,
    new: "libvlc_new" => unsafe extern "C" fn(c_int, *const *const c_char) -> Ptr,
    version: "libvlc_get_version" => unsafe extern "C" fn() -> *const c_char,
    media_new: "libvlc_media_new_location" => unsafe extern "C" fn(Ptr, *const c_char) -> Ptr,
    media_release: "libvlc_media_release" => unsafe extern "C" fn(Ptr),
    media_option: "libvlc_media_add_option" => unsafe extern "C" fn(Ptr, *const c_char),
    media_stats: "libvlc_media_get_stats" => unsafe extern "C" fn(Ptr, *mut MediaStats) -> c_int,
    tracks_get: "libvlc_media_tracks_get" => unsafe extern "C" fn(Ptr, *mut *mut *mut MediaTrack) -> u32,
    tracks_release: "libvlc_media_tracks_release" => unsafe extern "C" fn(*mut *mut MediaTrack, u32),
    player_new: "libvlc_media_player_new" => unsafe extern "C" fn(Ptr) -> Ptr,
    set_media: "libvlc_media_player_set_media" => unsafe extern "C" fn(Ptr, Ptr),
    player_release: "libvlc_media_player_release" => unsafe extern "C" fn(Ptr),
    play: "libvlc_media_player_play" => unsafe extern "C" fn(Ptr) -> c_int,
    stop: "libvlc_media_player_stop" => unsafe extern "C" fn(Ptr),
    pause: "libvlc_media_player_set_pause" => unsafe extern "C" fn(Ptr, c_int),
    time: "libvlc_media_player_get_time" => unsafe extern "C" fn(Ptr) -> i64,
    set_time: "libvlc_media_player_set_time" => unsafe extern "C" fn(Ptr, i64),
    length: "libvlc_media_player_get_length" => unsafe extern "C" fn(Ptr) -> i64,
    state: "libvlc_media_player_get_state" => unsafe extern "C" fn(Ptr) -> c_int,
    seekable: "libvlc_media_player_is_seekable" => unsafe extern "C" fn(Ptr) -> c_int,
    volume: "libvlc_audio_set_volume" => unsafe extern "C" fn(Ptr, c_int) -> c_int,
    mute: "libvlc_audio_set_mute" => unsafe extern "C" fn(Ptr, c_int),
    audio_tracks: "libvlc_audio_get_track_description" => unsafe extern "C" fn(Ptr) -> *mut Description,
    audio_get: "libvlc_audio_get_track" => unsafe extern "C" fn(Ptr) -> c_int,
    audio_set: "libvlc_audio_set_track" => unsafe extern "C" fn(Ptr, c_int) -> c_int,
    spu_tracks: "libvlc_video_get_spu_description" => unsafe extern "C" fn(Ptr) -> *mut Description,
    spu_get: "libvlc_video_get_spu" => unsafe extern "C" fn(Ptr) -> c_int,
    spu_set: "libvlc_video_set_spu" => unsafe extern "C" fn(Ptr, c_int) -> c_int,
    descriptions_release: "libvlc_track_description_list_release" => unsafe extern "C" fn(*mut Description),
    callbacks: "libvlc_video_set_callbacks" => unsafe extern "C" fn(Ptr, Option<Lock>, Option<Unlock>, Option<Display>, Ptr),
    format_callbacks: "libvlc_video_set_format_callbacks" => unsafe extern "C" fn(Ptr, Option<Format>, Option<Cleanup>)
}

#[repr(align(32))]
#[derive(Clone)]
struct Pixels([u8; 32]);
struct Frame { pixels: Vec<Pixels>, width: u32, height: u32, pitch: u32, sequence: u64, generation: u64 }
static FRAME: OnceLock<Mutex<Frame>> = OnceLock::new();
fn frame() -> &'static Mutex<Frame> { FRAME.get_or_init(|| Mutex::new(Frame { pixels: vec![], width: 0, height: 0, pitch: 0, sequence: 0, generation: 0 })) }

unsafe extern "C" fn format(_: *mut Ptr, chroma: *mut c_char, width: *mut u32, height: *mut u32, pitches: *mut u32, lines: *mut u32) -> u32 {
    unsafe {
        if width.is_null() || height.is_null() || chroma.is_null() || pitches.is_null() || lines.is_null() || *width == 0 || *height == 0 { return 0; }
        let scale = (MAX_WIDTH as f64 / *width as f64).min(MAX_HEIGHT as f64 / *height as f64).min(1.0);
        *width = ((*width as f64 * scale).round() as u32).clamp(1, MAX_WIDTH);
        *height = ((*height as f64 * scale).round() as u32).clamp(1, MAX_HEIGHT);
        let Ok(mut frame) = frame().lock() else { return 0; };
        frame.width = *width; frame.height = *height;
        frame.pitch = (*width * 4 + 31) / 32 * 32;
        let allocated_lines=(*height+31)/32*32;
        frame.pixels = vec![Pixels([0; 32]); (frame.pitch as usize * allocated_lines as usize) / 32];
        std::ptr::copy_nonoverlapping(b"RGBA".as_ptr(), chroma.cast(), 4);
        *pitches = frame.pitch; *lines = allocated_lines;
        1
    }
}
unsafe extern "C" fn lock(_: Ptr, planes: *mut Ptr) -> Ptr {
    let Ok(mut guard) = frame().lock() else { return std::ptr::null_mut(); };
    unsafe { *planes = guard.pixels.as_mut_ptr().cast(); }
    Box::into_raw(Box::new(guard)).cast()
}
unsafe extern "C" fn unlock(_: Ptr, picture: Ptr, _: *const Ptr) {
    if !picture.is_null() { unsafe { drop(Box::from_raw(picture.cast::<MutexGuard<'static, Frame>>())); } }
}
unsafe extern "C" fn display(_: Ptr, _: Ptr) { if let Ok(mut frame) = frame().lock() { frame.sequence = frame.sequence.wrapping_add(1); } }

struct Engine { lib: Library, instance: Ptr, player: Ptr, media: Ptr, generation: u64, audio: Vec<Value>, subs: Vec<Value> }
// LibVLC calls are serialized by ENGINE. Its video thread only uses FRAME.
unsafe impl Send for Engine {}
static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();
pub(crate) fn runtime() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundled = exe.parent()?.join("vlc/libvlc.dll");
    if bundled.is_file() { return Some(bundled); }
    let source_runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/vlc/libvlc.dll");
    source_runtime.is_file().then_some(source_runtime)
}
pub(crate) fn text(pointer: *const c_char) -> String {
    if pointer.is_null() { String::new() } else { unsafe { CStr::from_ptr(pointer).to_string_lossy().chars().take(200).collect() } }
}
fn track_language(metadata: Option<&String>, label: &str) -> String {
    if let Some(language)=metadata.filter(|language| !language.is_empty() && language.as_str()!="und") { return language.clone(); }
    // TS metadata can use sequential ES IDs while the active decoder exposes
    // PIDs. Its track description still carries the language of that exact PID.
    // Never join those two ID spaces by array position.
    let Some((_,language))=label.rsplit_once('[') else { return String::new(); };
    let language=language.trim_end_matches(']').trim().to_ascii_lowercase();
    match language.as_str() {
        "spanish"|"español"=>"spa", "english"|"inglés"=>"eng", "portuguese"|"portugués"=>"por",
        "french"|"français"=>"fra", "german"|"deutsch"=>"deu", "italian"|"italiano"=>"ita",
        "japanese"=>"jpn", "korean"=>"kor", "chinese"=>"zho", "russian"=>"rus",
        value if crate::models::valid_audio_language(value)=>value,
        _=>"",
    }.to_string()
}
pub(crate) fn diagnostic(stage:&str, data:Value) {
    if std::env::var_os("XUPER_PLAYBACK_DIAGNOSTIC").is_none() { return; }
    let path=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/runtime.safe.jsonl");
    if std::fs::metadata(&path).is_ok_and(|metadata|metadata.len()>128*1024) { return; }
    if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _=writeln!(file,"{}",json!({"stage":stage,"data":data}));
    }
}
impl Engine {
    fn new() -> Result<Self, String> {
        let began=std::time::Instant::now();
        let path = runtime().ok_or("El paquete no contiene el motor VLC. Vuelve a extraer la carpeta completa.")?;
        let lib = unsafe { Library::load(&path)? };
        diagnostic("library-loaded",json!({"elapsed_ms":began.elapsed().as_millis()}));
        let args = ["--ignore-config".to_string(), "--no-video-title-show".into(), "--no-osd".into(),
            "--no-media-library".into(), "--no-metadata-network-access".into(), "--no-sub-autodetect-file".into(),
            "--quiet".into()];
        let args: Vec<CString> = args.iter().filter_map(|arg| CString::new(arg.as_str()).ok()).collect();
        let pointers: Vec<_> = args.iter().map(|arg| arg.as_ptr()).collect();
        let instance = unsafe { (lib.new)(pointers.len() as i32, pointers.as_ptr()) };
        diagnostic("instance-created",json!({"elapsed_ms":began.elapsed().as_millis(),"ok":!instance.is_null()}));
        if instance.is_null() { return Err("VLC no pudo iniciar su motor de reproducción.".into()); }
        Ok(Self { lib, instance, player: std::ptr::null_mut(), media: std::ptr::null_mut(), generation: 0, audio: vec![], subs: vec![] })
    }
    fn stop(&mut self) {
        if !self.player.is_null() { unsafe { (self.lib.stop)(self.player); (self.lib.player_release)(self.player); } self.player = std::ptr::null_mut(); }
        if !self.media.is_null() { unsafe { (self.lib.media_release)(self.media); } self.media = std::ptr::null_mut(); }
        self.audio.clear(); self.subs.clear();
        if let Ok(mut frame) = frame().lock() { frame.width = 0; frame.height = 0; }
    }
    fn tracks(&mut self) {
        let mut metadata = std::collections::HashMap::new();
        unsafe {
            let mut rows = std::ptr::null_mut();
            let count = (self.lib.tracks_get)(self.media, &mut rows);
            if !rows.is_null() {
                for index in 0..count.min(256) as usize {
                    if let Some(row) = (*rows.add(index)).as_ref() { metadata.insert((row.kind, row.id), text(row.language)); }
                }
                (self.lib.tracks_release)(rows, count);
            }
            for kind in [0, 2] {
                let first = if kind == 0 { (self.lib.audio_tracks)(self.player) } else { (self.lib.spu_tracks)(self.player) };
                let mut current = first; let mut output = vec![];
                for _ in 0..128 {
                    let Some(row) = current.as_ref() else { break; };
                    if row.id >= 0 { let label=text(row.name);output.push(json!({"id": row.id, "language":track_language(metadata.get(&(kind, row.id)),&label), "label":label})); }
                    current = row.next;
                }
                if !first.is_null() { (self.lib.descriptions_release)(first); }
                if kind == 0 { self.audio = output; } else { self.subs = output; }
            }
        }
    }
    fn status(&mut self) -> Value {
        if self.player.is_null() { return json!({"active":false,"generation":self.generation}); }
        unsafe {
            self.tracks();
            let (width, height, frames) = frame().lock().map(|frame| (frame.width, frame.height, frame.sequence)).unwrap_or_default();
            let mut stats=MediaStats { values:[0;15] }; (self.lib.media_stats)(self.media,&mut stats);
            json!({"active":true,"generation":self.generation,"state":(self.lib.state)(self.player),
                "time":(self.lib.time)(self.player).max(0) as f64 / 1000.0,
                "duration":(self.lib.length)(self.player).max(0) as f64 / 1000.0,
                "seekable":(self.lib.seekable)(self.player)!=0,"width":width,"height":height,"frames":frames,
                "decoded_video_frames":stats.values[6],"decoded_audio_blocks":stats.values[7],"lost_frames":stats.values[9],
                "audio_tracks":self.audio,"audio_track":(self.lib.audio_get)(self.player),
                "subtitle_tracks":self.subs,"subtitle_track":(self.lib.spu_get)(self.player)})
        }
    }
}
pub(crate) fn cast_audio_tracks(lib:&Library,media:Ptr,player:Ptr)->Vec<Value>{
    let mut languages=std::collections::HashMap::new();let mut output=vec![];
    unsafe{
        let mut rows=std::ptr::null_mut();let count=(lib.tracks_get)(media,&mut rows);
        if !rows.is_null(){for index in 0..count.min(256) as usize{if let Some(row)=(*rows.add(index)).as_ref(){if row.kind==0{languages.insert(row.id,text(row.language));}}}(lib.tracks_release)(rows,count);}
        let first=(lib.audio_tracks)(player);let mut next=first;
        for _ in 0..128{let Some(row)=next.as_ref()else{break;};if row.id>=0{let label=text(row.name);output.push(json!({"id":row.id,"language":track_language(languages.get(&row.id),&label),"label":label}));}next=row.next;}
        if !first.is_null(){(lib.descriptions_release)(first);}
    }output
}
pub fn open(url: &str, params: &Value) -> Result<Value, String> {
    let began=std::time::Instant::now();
    let mut guard = ENGINE.get_or_init(||Mutex::new(None)).lock().map_err(|_| "VLC se interrumpió.")?;
    if guard.is_none() { *guard = Some(Engine::new()?); }
    let engine = guard.as_mut().unwrap(); engine.stop(); engine.generation += 1;
    if let Ok(mut frame)=frame().lock() { frame.generation=engine.generation; }
    let address = CString::new(url).map_err(|_| "La fuente local es inválida.")?;
    unsafe {
        engine.media = (engine.lib.media_new)(engine.instance, address.as_ptr());
        if engine.media.is_null() { return Err("VLC no pudo preparar la película.".into()); }
        for option in [":network-caching=1200", ":file-caching=300", ":http-reconnect", ":no-sub-autodetect-file", ":sub-track=-1"] {
            (engine.lib.media_option)(engine.media, CString::new(option).unwrap().as_ptr());
        }
        engine.player = (engine.lib.player_new)(engine.instance);
        if engine.player.is_null() { engine.stop(); return Err("VLC no pudo abrir el reproductor.".into()); }
        (engine.lib.callbacks)(engine.player, Some(lock), Some(unlock), Some(display), std::ptr::null_mut());
        (engine.lib.format_callbacks)(engine.player, Some(format), None);
        (engine.lib.set_media)(engine.player, engine.media);
        (engine.lib.volume)(engine.player, (params["volume"].as_f64().unwrap_or(1.0).clamp(0.0,1.0)*100.0).round() as i32);
        (engine.lib.mute)(engine.player, i32::from(params["muted"].as_bool().unwrap_or(false)));
        if (engine.lib.play)(engine.player) != 0 { engine.stop(); return Err("VLC no pudo iniciar la película.".into()); }
    }
    diagnostic("play-called",json!({"elapsed_ms":began.elapsed().as_millis(),"generation":engine.generation}));
    Ok(json!({"generation":engine.generation,"engine":"libVLC","version":text(unsafe{(engine.lib.version)()})}))
}
pub fn command(command: &str, params: &Value) -> Result<Value, String> {
    if command == "vlc.capabilities" { return Ok(json!({"available":runtime().is_some(),"engine":"libVLC","shared_buffer":true})); }
    let mut guard = ENGINE.get_or_init(||Mutex::new(None)).lock().map_err(|_| "VLC se interrumpió.")?;
    let Some(engine) = guard.as_mut() else { return Ok(json!({"active":false})); };
    if command == "vlc.status" { return Ok(engine.status()); }
    if params["generation"].as_u64() != Some(engine.generation) { return Err("Esta reproducción de VLC ya terminó.".into()); }
    if command == "vlc.stop" { engine.stop(); return Ok(json!({"stopped":true})); }
    if engine.player.is_null() { return Err("No hay una película abierta en VLC.".into()); }
    unsafe {
        match command {
            "vlc.pause" => (engine.lib.pause)(engine.player, i32::from(params["paused"].as_bool().unwrap_or(true))),
            "vlc.seek" => {
                let seconds = params["time"].as_f64().filter(|v|v.is_finite() && *v >= 0.0 && *v <= 7.0*86400.0).ok_or("La posición no es válida.")?;
                if (engine.lib.seekable)(engine.player)==0 {return Err("Esta fuente todavía no permite cambiar de posición.".into());}
                (engine.lib.set_time)(engine.player, (seconds*1000.0).round() as i64);
            }
            "vlc.volume" => {
                let volume = params["volume"].as_f64().filter(|v| v.is_finite()).ok_or("El volumen no es válido.")?;
                (engine.lib.volume)(engine.player,(volume.clamp(0.0,1.0)*100.0).round() as i32);
                (engine.lib.mute)(engine.player,i32::from(params["muted"].as_bool().unwrap_or(false)));
            }
            "vlc.audio" | "vlc.subtitle" => {
                let id = params["track"].as_i64().and_then(|value|i32::try_from(value).ok()).ok_or("La pista no es válida.")?;
                engine.tracks();
                let tracks = if command == "vlc.audio" { &engine.audio } else { &engine.subs };
                if !(id == -1 && command == "vlc.subtitle") && !tracks.iter().any(|track|track["id"].as_i64()==Some(id as i64)) {
                    return Err("La película no contiene esa pista.".into());
                }
                let result = if command == "vlc.audio" { (engine.lib.audio_set)(engine.player,id) } else { (engine.lib.spu_set)(engine.player,id) };
                if result != 0 { return Err("VLC no pudo cambiar a esa pista.".into()); }
            }
            _ => return Err("Control VLC no admitido.".into()),
        }
    }
    Ok(engine.status())
}
pub fn shutdown() { if let Some(engine) = ENGINE.get() { if let Ok(mut guard) = engine.lock() { if let Some(engine) = guard.as_mut() { engine.stop(); } } } }

/// Owned on the UI thread. One outstanding frame at a time: the JS acknowledgement
/// arrives only after putImageData has synchronously copied the shared pixels.
pub struct Frames { buffer: Option<ICoreWebView2SharedBuffer>, pointer: *mut u8, waiting: Option<u64>, sent: u64 }
impl Frames {
    pub fn new() -> Self { Self { buffer:None, pointer:std::ptr::null_mut(), waiting:None, sent:0 } }
    pub fn prepare(&mut self, webview: &wry::WebView) -> Result<Value, String> {
        unsafe {
            if self.buffer.is_none() {
                let env = webview.environment().cast::<ICoreWebView2Environment12>().map_err(|_|"Actualiza WebView2 para usar VLC integrado.")?;
                let buffer = env.CreateSharedBuffer(BUFFER_BYTES as u64).map_err(|_|"No se pudo preparar la imagen de VLC.")?;
                buffer.Buffer(&mut self.pointer).map_err(|_|"No se pudo abrir la imagen de VLC.")?;
                self.buffer = Some(buffer);
            }
            let metadata: Vec<u16> = "{\"kind\":\"nebula-vlc-pixels\"}".encode_utf16().chain(Some(0)).collect();
            webview.webview().cast::<ICoreWebView2_17>().map_err(|_|"Actualiza WebView2 para usar VLC integrado.")?
                .PostSharedBufferToScript(self.buffer.as_ref().unwrap(), COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY, PCWSTR(metadata.as_ptr()))
                .map_err(|_|"No se pudo conectar la imagen de VLC con la interfaz.")?;
        }
        self.waiting = None;
        Ok(json!({"prepared":true}))
    }
    pub fn acknowledge(&mut self, sequence: u64) { if self.waiting == Some(sequence) { self.waiting = None; } }
    pub fn draw(&mut self, webview:&wry::WebView) {
        if self.buffer.is_none() || self.waiting.is_some() || self.pointer.is_null() { return; }
        let Ok(frame) = frame().try_lock() else { return; };
        let bytes = frame.pitch as usize * frame.height as usize;
        if frame.sequence == self.sent || bytes == 0 || bytes > BUFFER_BYTES || frame.pixels.len()*32 < bytes { return; }
        unsafe { std::ptr::copy_nonoverlapping(frame.pixels.as_ptr().cast::<u8>(), self.pointer, bytes); }
        let message = json!({"kind":"nebula-vlc-frame","sequence":frame.sequence,"generation":frame.generation,"width":frame.width,"height":frame.height,"stride":frame.pitch});
        let wide:Vec<u16> = message.to_string().encode_utf16().chain(Some(0)).collect();
        if unsafe { webview.webview().PostWebMessageAsJson(PCWSTR(wide.as_ptr())) }.is_ok() {
            self.sent = frame.sequence; self.waiting = Some(frame.sequence);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pid_track_language_uses_its_description_when_metadata_ids_differ() {
        assert_eq!(track_language(None,"Track 1 - [Spanish]"),"spa");
        assert_eq!(track_language(None,"Track 2 - [English]"),"eng");
        assert_eq!(track_language(Some(&"fra".into()),"Track 1 - [Spanish]"),"fra");
        assert_eq!(track_language(None,"Director commentary"),"");
        assert_eq!(track_language(None,"Track 3 - [Unknown]"),"");
    }
    #[test]
    fn libvlc3_abi_and_pixel_alignment() {
        assert_eq!(std::mem::size_of::<MediaTrack>(),56);
        assert_eq!(std::mem::offset_of!(MediaTrack,language),40);
        assert_eq!(std::mem::size_of::<Description>(),24);
        assert_eq!(std::mem::align_of::<Pixels>(),32);
        let (mut width,mut height,mut pitch,mut lines)=(854,356,0,0);
        let mut chroma=[0i8;4]; let mut opaque=std::ptr::null_mut();
        assert_eq!(unsafe{format(&mut opaque,chroma.as_mut_ptr(),&mut width,&mut height,&mut pitch,&mut lines)},1);
        assert_eq!((width,height),(854,356));
        assert_eq!(pitch%32,0); assert_eq!(lines%32,0);
        assert!(pitch as usize * height as usize <= BUFFER_BYTES);
    }
    #[test]
    #[ignore = "real bundled VLC diagnostic, no service requests"]
    fn bundled_vlc_switches_real_audio_streams() {
        let file=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/two-language-audio.mka");
        let url=reqwest::Url::from_file_path(file).unwrap().to_string();
        let opened=open(&url,&json!({"volume":0,"muted":true})).unwrap();
        let generation=opened["generation"].as_u64().unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(12);
        let before=loop {
            let status=command("vlc.status",&json!({})).unwrap();
            if status["audio_tracks"].as_array().is_some_and(|tracks|tracks.len()==2) && status["decoded_audio_blocks"].as_u64().unwrap_or(0)>0 { break status; }
            assert!(std::time::Instant::now()<deadline,"VLC did not decode the fixture: {status}");
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let spanish=before["audio_tracks"].as_array().unwrap().iter().find(|track|track["language"]=="spa" || track["language"]=="es").unwrap()["id"].as_i64().unwrap();
        let selected=command("vlc.audio",&json!({"generation":generation,"track":spanish})).unwrap();
        assert_eq!(selected["audio_track"],spanish);
        assert!(command("vlc.audio",&json!({"generation":generation,"track":999999})).is_err());
        let report=json!({"passed":true,"engine":"libVLC","version":opened["version"],"synthetic_fixture":true,
            "real_service_requests":0,"audio_tracks":before["audio_tracks"],"selected_spanish_track":spanish,
            "decoded_audio_blocks":before["decoded_audio_blocks"],"invalid_track_rejected":true});
        std::fs::write(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/multiaudio.safe.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        command("vlc.stop",&json!({"generation":generation})).unwrap();
        println!("{report}");
    }
}
