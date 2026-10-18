//! Stateful application API. Server requests are serialized so first-launch IPC
//! commands cannot enroll the same installation concurrently.
use crate::models::{self, CatalogPage, Category, Episode, Item, ItemContext, Session};
use crate::protocol::Protocol;
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::HashMap, fs, path::{Path, PathBuf}, sync::{Mutex, OnceLock}, time::{Duration, Instant}};

static API: OnceLock<Mutex<Option<Api>>> = OnceLock::new();
static MEDIA_PROXY: OnceLock<crate::media_proxy::MediaProxy> = OnceLock::new();
const PAGE_SIZE: u64 = 50;
const PLAYBACK_LIMITATION: &str = "La reproducción de este título todavía no está disponible en Windows.";
const PLAY_AUTH_TTL: Duration = Duration::from_secs(60);
const PLAY_AUTH_MAX_ENTRIES: usize = 8;
const PLAY_AUTH_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
struct PlayAuthorizationKey {
    content_id: String,
    column_id: Option<i64>,
    series_content_id: Option<String>,
    episode_number: Option<i64>,
}
struct PlayAuthorization { received: Instant, bytes: usize, data: Value }

struct Api {
    protocol: Protocol,
    directory: PathBuf,
    session: Session,
    initialized: bool,
    session_replaced: bool,
    boot_error: Option<(Instant, String)>,
    reconnect_attempt: Option<(Instant, Option<String>)>,
    playback_warning: Option<String>,
    slb: Option<Value>,
    slb_valid_until: Option<Instant>,
    ranger_identity: Option<String>,
    deadline: Option<Instant>,
    categories: HashMap<String, (Instant, Vec<Category>)>,
    pages: HashMap<String, (Instant, CatalogPage)>,
    filter_choices: Option<(Instant,Value)>,
    items: HashMap<String, ItemContext>,
    play_authorizations: HashMap<PlayAuthorizationKey, PlayAuthorization>,
    favorites: Vec<Item>,
}

pub fn handle(command: &str, params: &Value) -> Result<Value, String> {
    if command.starts_with("cast.") { return crate::native_cast::command(command,params); }
    if command.starts_with("update.") { return crate::updater::command(command); }
    if command == "vlc.open" {
        let session = required(params,"session_id")?;
        let ticket = MEDIA_PROXY.get().ok_or("Primero prepara la reproducción.")?.native_ticket(session)?;
        return crate::native_player::open(&ticket.url, params);
    }
    if command.starts_with("vlc.") { return crate::native_player::command(command,params); }
    if command=="playback.stop" {
        let session_id=required(params,"session_id")?;
        return Ok(json!({"stopped":MEDIA_PROXY.get().is_some_and(|proxy|proxy.release(session_id))}));
    }
    if !["status", "session.reconnect", "categories", "catalog", "search", "details", "filters.options", "filters.catalog", "playback", "playback.options", "playback.subtitles", "playback.subtitle", "playback.report", "favorites.list", "favorites.toggle"].contains(&command) {
        return Err("Operación desconocida.".into());
    }
    let mut guard = API.get_or_init(|| Mutex::new(None)).lock()
        .map_err(|_| "La sesión local se interrumpió; vuelve a abrir la aplicación.".to_string())?;
    if guard.is_none() { *guard = Some(Api::new()?); }
    let api = guard.as_mut().unwrap();
    if command == "details" && params["season_switch"] == true {
        if let Some(context) = api.items.get_mut(&item_key(kind(params)?,required(params,"id")?)) {
            context.column_id=None; context.item.source_category=None;
        }
    }
    if command == "playback.subtitle" {
        let track_id = required(params,"track_id")?;
        let source = api.subtitle_sources(kind(params)?, required(params,"id")?)?.into_iter()
            .find(|source| source.track.id == track_id)
            .ok_or("La pista ya no está disponible. Vuelve a abrir el menú de subtítulos.")?;
        // Download independently of the portal mutex so catalog operations and
        // video cancellation remain responsive while a subtitle is loading.
        drop(guard);
        return Ok(json!({"vtt":crate::subtitles::fetch_vtt(source.url)?}));
    }
    match command {
        "status" => api.status(),
        "session.reconnect" => api.reconnect_session(),
        "categories" => serialize(api.categories(kind(params)?)?),
        "catalog" => serialize(api.catalog(kind(params)?, params)?),
        "search" => serialize(api.search(kind(params)?, params)?),
        "filters.options" => api.filter_options(),
        "filters.catalog" => serialize(api.filtered_catalog(params)?),
        "details" => serialize(api.details(kind(params)?, required(params,"id")?)?),
        "playback" => api.playback(kind(params)?, required(params,"id")?, params),
        "playback.options" => serialize(api.playback_options(kind(params)?, required(params,"id")?, params)?),
        "playback.subtitles" => Ok(json!({"tracks":api.subtitle_sources(kind(params)?, required(params,"id")?)?.into_iter().map(|source|source.track).collect::<Vec<_>>()})),
        "playback.report" => api.playback_report(params),
        "favorites.list" => serialize(&api.favorites),
        "favorites.toggle" => api.toggle_favorite(params),
        _ => unreachable!(),
    }
}

/// The desktop IPC uses this exact classification; no server text or token is
/// needed to tell the frontend that the user must explicitly reconnect.
pub fn is_session_expired_error(message: &str) -> bool {
    crate::protocol::is_session_expired_error(message)
}

/// Fixed media identity and optional preferences are the only input to Cast's
/// resolver. No browser request may supply a URL, license or engine track ID.
pub(crate) fn native_cast_ticket(session:&str)->Result<crate::media_proxy::PlaybackTicket,String>{
    MEDIA_PROXY.get().ok_or("No se pudo localizar el video local.")?.native_ticket(session)
}
pub(crate) fn cast_playback_params(kind:&str,id:&str,quality:Option<&str>,audio_language:Option<&str>)->Result<Value,String>{
    let params=json!({"kind":kind,"id":id,"quality":quality,"audio_language":audio_language});
    playback_preferences(&params)?;
    if kind=="live" && (quality.is_some() || audio_language.is_some()){
        return Err("Esta señal en vivo no ofrece opciones de calidad o idioma.".into());
    }
    Ok(params)
}

#[cfg(test)]
pub fn cast_playback(kind:&str,id:&str,quality:Option<&str>,audio_language:Option<&str>,relay:&crate::media_proxy::MediaProxy)->Result<crate::cast::PreparedMedia,String>{
    let params=cast_playback_params(kind,id,quality,audio_language)?;
    let value=handle("playback",&params)?;
    let session_id=required(&value,"session_id")?.to_string();
    let proxy=MEDIA_PROXY.get().ok_or("No se pudo localizar la reproducción local para Chromecast.")?;
    let result=(||{
        let url=required(&value,"url")?.to_string();
        let parsed=reqwest::Url::parse(&url).map_err(|_|"El enlace de reproducción local no es válido.")?;
        let loopback=parsed.host_str().and_then(|host|host.parse::<std::net::IpAddr>().ok()).is_some_and(|address|address.is_loopback());
        if parsed.scheme()!="http" || !loopback || !parsed.username().is_empty() || parsed.password().is_some(){
            return Err("El enlace de reproducción no pertenece al reproductor local.".into());
        }
        let ticket=crate::media_proxy::PlaybackTicket{url,mime_type:required(&value,"mime_type")?.to_string(),session_id:session_id.clone()};
        // The relay additionally verifies ownership, exact URL, active session
        // and the stored result of its bounded MP4/HLS media probe.
        let selection=value.get("selection").filter(|selected|selected.is_object()).map(|selected|json!({
            "quality":selected.get("quality").and_then(Value::as_str),
            "audio_language":selected.get("audio_language").and_then(Value::as_str),
            "audio_track_index":selected.get("audio_track_index").and_then(Value::as_u64)
        })).unwrap_or(Value::Null);
        Ok(crate::cast::PreparedMedia{ticket:proxy.export_session_into(relay,&ticket)?,selection})
    })();
    // Export creates an independent session. Always release the temporary
    // desktop session, including validation or export failures.
    proxy.release(&session_id);
    result
}

impl Api {
    fn new() -> Result<Self, String> {
        Self::from_directory(data_directory()?, Protocol::new()?)
    }

    fn from_directory(directory: PathBuf, protocol: Protocol) -> Result<Self, String> {
        fs::create_dir_all(&directory).map_err(|_| "No se pudo crear la carpeta de datos de la aplicación.".to_string())?;
        let session_path = directory.join("session.json");
        let session: Session = if session_path.exists() {
            read_json(&session_path)?
        } else {
            let value = Session {
                installation_id: uuid::Uuid::new_v4().simple().to_string(), sn:String::new(), sn_token:String::new(),
                user_id:String::new(), user_token:String::new(), portal_code:String::new(), has_pay:String::new(), user_identity:String::new(),
            };
            save_json(&session_path, &value)?;
            value
        };
        if session.installation_id.len() != 32 || !session.installation_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("La identidad local de instalación es inválida. Se conservó el archivo sin modificarlo.".into());
        }
        let favorites_path = directory.join("favorites.json");
        let favorites: Vec<Item> = if favorites_path.exists() { read_json(&favorites_path)? } else { vec![] };
        Ok(Self { protocol, directory, session, initialized:false, session_replaced:false,
            boot_error:None, reconnect_attempt:None,
            playback_warning:None, slb:None, slb_valid_until:None, ranger_identity:None, deadline:None, categories:HashMap::new(), pages:HashMap::new(), filter_choices:None, items:HashMap::new(), play_authorizations:HashMap::new(), favorites })
    }

    fn save_session(&self) -> Result<(), String> { save_json(&self.directory.join("session.json"), &self.session) }

    fn ensure_session(&mut self) -> Result<(), String> {
        if self.session_replaced { return Err(crate::protocol::SESSION_REPLACED_MESSAGE.into()); }
        if self.initialized { return Ok(()); }
        if let Some((when, message)) = &self.boot_error {
            if when.elapsed() < Duration::from_secs(20) { return Err(message.clone()); }
        }
        let result = self.initialize_session();
        if let Err(message) = &result {
            if is_session_expired_error(message) { self.invalidate_replaced_session(); }
            self.boot_error = Some((Instant::now(), message.clone()));
        }
        result
    }

    fn initialize_session(&mut self) -> Result<(), String> {
        // Reuse the server-issued token across launches. Activating the same
        // installation again replaces tokens held by an existing window.
        let complete = !self.session.sn.is_empty() && !self.session.user_id.is_empty()
            && !self.session.user_token.is_empty() && !self.session.portal_code.is_empty();
        if !complete { self.activate_session()?; }
        self.validate_session()
    }

    fn activate_session(&mut self) -> Result<(), String> {
        if self.session.sn.is_empty() {
            // Android-only hardware identifiers are unavailable on Windows. The
            // compatibility field androidId contains this client's own random,
            // persistent installation ID, never a copied device or account ID.
            let mut request = json!({
                "androidId":&self.session.installation_id[..16],"wifiMac":"","etheMac":"","serialNumber":"",
                "diskInfo":"","ramSize":"","romSize":"","gatewayMac":"","cpuId":"",
                "se":self.protocol.source_apk_certificate_digest(),
                "display":"Windows Desktop","brand":"","verId":"Windows","cpuAbi":std::env::consts::ARCH,
                "hardware":std::env::consts::ARCH,"manufacturer":"","fingerprint":"","host":"",
                "device":"Windows Desktop","board":"","tags":""
            });
            // No OS process is launched and no machine serial number is collected.
            if let Some(ram) = physical_memory_bytes() { request["ramSize"] = json!(format!("{:.1} GB",ram as f64/1_000_000_000.0)); }
            let data = self.protocol.post_until("o", request, "", self.deadline)?;
            let token = models::text(&data,"snToken").ok_or_else(|| "La respuesta de instalación no incluye snToken.".to_string())?;
            let is_new = models::text(&data,"isNew").as_deref() == Some("1");
            self.session.sn = if is_new {
                models::text(&data,"sn").unwrap_or_else(|| self.protocol.sn_digest(&token))
            } else { self.protocol.sn_digest(&token) };
            self.session.sn_token = if is_new { String::new() } else { token };
            self.save_session()?;
        }
        // Activation is limited to initial enrollment or explicit reconnect.
        // Existing installation/SN and favorites are always retained.
        let data = self.protocol.post_until("D", json!({"snToken":self.session.sn_token,"macAddr":"","channel":"default"}), &self.session.sn, self.deadline)?;
        let user_id = models::text(&data,"userId").ok_or_else(|| "El servidor no entregó una identidad de sesión.".to_string())?;
        let user_token = models::text(&data,"userToken").ok_or_else(|| "El servidor no entregó un token de sesión.".to_string())?;
        let portal = data.get("portalCodeList").and_then(Value::as_array).and_then(|v|v.first())
            .and_then(|v|models::text(v,"portalCode"))
            .ok_or_else(|| "El servidor no autorizó ningún catálogo para esta instalación.".to_string())?;
        self.session.user_id = user_id;
        self.session.user_token = user_token;
        self.session.portal_code = portal;
        self.session.has_pay = models::text(&data,"hasPay").unwrap_or_default();
        self.session.user_identity = models::text(&data,"userIdentity").unwrap_or_default();
        self.ranger_identity = None;
        self.play_authorizations.clear();
        self.save_session()?;
        Ok(())
    }

    fn validate_session(&mut self) -> Result<(), String> {
        let auth = self.authenticated_post("e0", json!({"type":"3","lang":"es"}))?;
        if let Some(value) = models::text(&auth,"hasPay") { self.session.has_pay = value; }
        if let Some(value) = models::text(&auth,"userIdentity") { self.session.user_identity = value; }
        self.save_session()?;
        // Catalog operations need the portal session. Playback bootstrap is
        // fetched only if K0 does not supply an explicit direct media URL.
        self.playback_warning = Some("Catálogo conectado.".into());
        self.initialized = true;
        self.session_replaced = false;
        self.boot_error = None;
        Ok(())
    }

    fn invalidate_replaced_session(&mut self) {
        self.initialized = false;
        self.session_replaced = true;
        self.categories.clear();
        self.pages.clear();
        self.filter_choices=None;
        self.items.clear();
        self.play_authorizations.clear();
        self.slb = None;
        self.slb_valid_until = None;
        self.ranger_identity = None;
        self.playback_warning = None;
    }

    fn status_value(&self) -> Value {
        if self.session_replaced {
            return json!({"connected":false,"ready":false,"session":"replaced","error_code":"session_expired",
                "platform":"Windows","message":crate::protocol::SESSION_REPLACED_MESSAGE,"playback_ready":false,
                "capabilities":["favorites","session.reconnect"]});
        }
        json!({"connected":self.initialized,"ready":self.initialized,"session":if self.initialized {"active"} else {"pending"},
            "platform":"Windows","source":"Xuper Mobile NF 6.5.5","version":env!("CARGO_PKG_VERSION"),
            "message":self.playback_warning.as_deref().unwrap_or("Catálogo conectado."),"playback_ready":false,
            "capabilities":["categories","catalog","search","details","favorites"]})
    }

    fn status(&mut self) -> Result<Value, String> {
        match self.ensure_session() {
            Ok(()) => Ok(self.status_value()),
            Err(error) if is_session_expired_error(&error) => Ok(self.status_value()),
            Err(error) => Err(error),
        }
    }

    fn reconnect_session(&mut self) -> Result<Value, String> {
        // A queued duplicate after success is a read of the active state. A
        // persisted token not yet validated is checked before any activation.
        if !self.session_replaced {
            match self.ensure_session() {
                Ok(()) => return Ok(self.status_value()),
                Err(error) if is_session_expired_error(&error) => {},
                Err(error) => return Err(error),
            }
        }
        if let Some((when, error)) = &self.reconnect_attempt {
            if when.elapsed() < Duration::from_secs(20) {
                return Err(error.clone().unwrap_or_else(|| "Espera unos segundos antes de volver a reconectar.".into()));
            }
        }
        let started = Instant::now();
        self.reconnect_attempt = Some((started, None));
        let previous_deadline = self.deadline;
        self.deadline = Some(started + Duration::from_secs(45));
        self.session_replaced = false;
        self.initialized = false;
        let result = self.activate_session().and_then(|()| self.validate_session());
        self.deadline = previous_deadline;
        match result {
            Ok(()) => Ok(self.status_value()),
            Err(error) => {
                // Keep recovery explicit even if the reconnect itself timed
                // out: queued catalog/status commands must not activate again.
                self.invalidate_replaced_session();
                self.reconnect_attempt = Some((started, Some(error.clone())));
                Err(error)
            },
        }
    }

    fn authenticated_post(&mut self, method: &str, body: Value) -> Result<Value, String> {
        if self.session_replaced { return Err(crate::protocol::SESSION_REPLACED_MESSAGE.into()); }
        if self.session.user_id.is_empty() || self.session.user_token.is_empty() || self.session.portal_code.is_empty() {
            return Err("No hay una sesión autorizada para esta operación.".into());
        }
        let mut object = body.as_object().cloned().ok_or_else(|| "Solicitud interna inválida.".to_string())?;
        object.insert("userId".into(),json!(self.session.user_id));
        object.insert("userToken".into(),json!(self.session.user_token));
        object.insert("portalCode".into(),json!(self.session.portal_code));
        let result = self.protocol.post_until(method,Value::Object(object),&self.session.sn,self.deadline);
        if result.as_ref().err().is_some_and(|error| is_session_expired_error(error)) {
            self.invalidate_replaced_session();
        }
        result
    }

    fn refresh_playback_bootstrap(&mut self)->Result<(),String>{
        let value=self.authenticated_post("y",json!({"encMediaSupported":1,"liveCodeList":[format!("{}_live",self.session.portal_code)],
            "type":"merge","appVer":self.protocol.api_version().to_string(),"lang":"pt","reserve1":"","pipFlag":"0",
            "hasPay":self.session.has_pay,"userIdentity":self.session.user_identity}))?;
        if models::integer(&value,"error_code").is_some_and(|code|code!=0){return Err("El servidor no autorizó la configuración del reproductor.".into());}
        if value.get("cdn_list").and_then(Value::as_array).is_none_or(|entries|entries.is_empty()){
            return Err("El servidor no entregó servidores de reproducción.".into());
        }
        // q5.i.B refreshes invalidTime seconds minus 600, with a 600s floor.
        // Windows refreshes on demand instead of running a background timer.
        let lifetime=models::integer(&value,"invalidTime").filter(|value|*value>=0)
            .map(|value|value.saturating_sub(600).max(600) as u64).unwrap_or(1800);
        self.slb_valid_until=Instant::now().checked_add(Duration::from_secs(lifetime));
        self.slb=Some(value);
        self.ranger_identity=None;
        Ok(())
    }

    fn categories(&mut self, kind: &str) -> Result<Vec<Category>, String> {
        self.ensure_session()?;
        if let Some((when, value)) = self.categories.get(kind) {
            if when.elapsed()<Duration::from_secs(300) { return Ok(value.clone()); }
        }
        let roots = self.authenticated_post("Z",json!({"pageNum":1,"pageSize":1000}))?;
        let root_list = array(&roots,"childColumnList")?;
        let expected = format!("{}_{}",self.session.portal_code,kind);
        let root = root_list.iter().find(|row| models::text(row,"code").as_deref()==Some(&expected))
            .ok_or_else(|| format!("El servidor no ofreció la sección {kind} para esta sesión."))?.clone();
        let root_id = models::integer(&root,"id").ok_or_else(|| "La sección del catálogo no tiene identificador válido.".to_string())?;
        let children = self.authenticated_post("Z",json!({"columnId":root_id,"specialFlag":"0","pageNum":1,"pageSize":1000}))?;
        let rows = array(&children,"childColumnList")?;
        let values = if rows.is_empty() { vec![root] } else { rows.clone() };
        let mut result=Vec::new();
        if kind=="vod" && values.len()>64 { return Err("La jerarquía del catálogo excede el límite de secciones compatible.".into()); }
        for row in &values {
            let Some(category)=category_from_row(row,kind,None) else { continue; };
            if kind=="vod" {
                // The VOD root contains sections; shelves are one level below
                // (j6.b4). A section itself legitimately has an empty assetList.
                let column=category.id.parse::<i64>().map_err(|_|"La sección no tiene un identificador válido.".to_string())?;
                let data=self.authenticated_post("Z",json!({"columnId":column,"specialFlag":"0","pageNum":1,"pageSize":1000}))?;
                let shelves=array(&data,"childColumnList")?;
                if !shelves.is_empty() {
                    result.extend(shelves.iter().filter_map(|shelf|category_from_row(shelf,kind,Some(&category))));
                    continue;
                }
            }
            result.push(category);
        }
        if !values.is_empty() && result.is_empty() { return Err("Las categorías recibidas no tienen el formato esperado.".into()); }
        self.categories.insert(kind.to_string(),(Instant::now(),result.clone()));
        Ok(result)
    }

    fn catalog(&mut self, kind: &str, params: &Value) -> Result<CatalogPage,String> {
        self.ensure_session()?;
        let page = page(params)?;
        let category = match models::text(params,"category") {
            Some(value) => value,
            None => {
                let categories=self.categories(kind)?;
                if categories.len()>1 { return Err("Selecciona una categoría para ver su catálogo.".into()); }
                categories.first().map(|c|c.id.clone()).ok_or_else(|| "No hay categorías disponibles.".to_string())?
            },
        };
        let column = category.parse::<i64>().map_err(|_| "La categoría seleccionada es inválida.".to_string())?;
        if column<0 { return Err("La categoría seleccionada es inválida.".into()); }
        let cache_key = format!("{kind}:{column}:{page}");
        if let Some((when,value))=self.pages.get(&cache_key) {
            if when.elapsed()<Duration::from_secs(60) { return Ok(value.clone()); }
        }
        let data = if kind=="live" {
            self.authenticated_post("i",json!({"columnId":column,"dataVersion":"","expireTimeStr":"","pageNum":page,"pageSize":PAGE_SIZE}))?
        } else {
            self.authenticated_post("x0",json!({"columnId":column,"columnType":"2","pageNum":page,"pageSize":PAGE_SIZE}))?
        };
        let key = if kind=="live" { "channelList" } else { "assetList" };
        let rows = array(&data,key)?;
        let items = self.remember_items(rows,kind,Some(column),Some(page))?;
        let has_more = catalog_has_more(kind,&data,page,rows.len());
        let result = CatalogPage{items,page,has_more,notice:None};
        if self.pages.len()>100 { self.pages.clear(); }
        self.pages.insert(cache_key,(Instant::now(),result.clone()));
        Ok(result)
    }

    fn search(&mut self, kind: &str, params: &Value) -> Result<CatalogPage,String> {
        self.ensure_session()?;
        let query=required(params,"query")?.trim();
        if query.is_empty() || query.len()>300 { return Err("Escribe una búsqueda de hasta 300 caracteres.".into()); }
        let page=page(params)?;
        if kind=="live" {
            // The observed searchByName(type=1) is VOD. Bound live search to
            // already loaded channels rather than inventing an API or crawling.
            let needle=query.to_lowercase();
            let mut all:Vec<Item>=self.items.values().filter(|context|context.item.kind=="live" && context.item.title.to_lowercase().contains(&needle)).map(|context|context.item.clone()).collect();
            all.sort_by(|a,b|a.id.cmp(&b.id));
            all.dedup_by(|a,b|a.id==b.id);
            let start=((page-1)*PAGE_SIZE) as usize;
            let has_more=start+(PAGE_SIZE as usize)<all.len();
            return Ok(CatalogPage{items:all.into_iter().skip(start).take(PAGE_SIZE as usize).collect(),page,has_more,notice:Some("Búsqueda en los canales ya cargados. Abre otras categorías para incluir más canales.".into())});
        }
        let data=self.authenticated_post("g",json!({"value":query,"type":"1","pageSize":PAGE_SIZE,"pageNum":page}))?;
        let groups=array(&data,"searchItemList")?;
        let mut rows=Vec::new();
        for group in groups { rows.extend(array(group,"itemList")?.iter().cloned()); }
        let items=self.remember_items(&rows,"vod",None,None)?;
        let has_more=models::integer(&data,"totalSize").map(|total|page*PAGE_SIZE<total.max(0) as u64).unwrap_or(rows.len() as u64==PAGE_SIZE);
        Ok(CatalogPage{items,page,has_more,notice:None})
    }

    fn filter_options(&mut self)->Result<Value,String>{
        self.ensure_session()?;
        if let Some((when,choices))=&self.filter_choices {
            if when.elapsed()<Duration::from_secs(300){return Ok(choices.clone());}
        }
        // REA: FilterGenreBean / FilterInfo; preserve the service's opaque keys.
        let data=self.authenticated_post("H",json!({"language":"es"}))?;
        let mut result=serde_json::Map::new();
        for (field,source) in [("genre","tags"),("year","year"),("country","originalCountry"),
            ("audio","audio"),("score","fixRating"),("type","types")] {
            let choices:Vec<_>=data.get(source).and_then(Value::as_array).into_iter().flatten().take(1000).filter_map(|row|{
                let (value,label)=if let Some(value)=row.as_str(){(value.to_string(),value.to_string())}
                    else{(models::text(row,"key")?,models::text(row,"value")?)};
                if value.len()>200 || label.len()>300{return None;}
                Some(json!({"value":value,"label":label}))
            }).collect();
            result.insert(field.into(),json!(choices));
        }
        // This portal supplies dynamic genres/years/countries/audio but omits
        // fixRating. These are numeric minimum thresholds, not invented scores.
        if result.get("score").and_then(Value::as_array).is_none_or(Vec::is_empty){
            result.insert("score".into(),json!((1..=9).rev().map(|score|json!({"value":score.to_string(),"label":format!("{score} o más")})).collect::<Vec<_>>()));
        }
        if result.get("type").and_then(Value::as_array).is_none_or(Vec::is_empty){
            // programType values observed in ShelveAsset, not the separate 0/1 type.
            result.insert("type".into(),json!([{"value":"movie","label":"Películas"},{"value":"teleplay","label":"Series"}]));
        }
        let result=Value::Object(result);self.filter_choices=Some((Instant::now(),result.clone()));Ok(result)
    }

    fn filtered_catalog(&mut self,params:&Value)->Result<CatalogPage,String>{
        let page=page(params)?;let choices=self.filter_options()?;
        let body=filter_request(params,&choices,page)?;
        let cache_key=format!("filters:{}",body);
        if let Some((when,result))=self.pages.get(&cache_key){if when.elapsed()<Duration::from_secs(60){return Ok(result.clone());}}
        let data=self.authenticated_post("z0",body)?;
        let rows=array(&data,"assetList")?;
        let items=self.remember_items(rows,"vod",None,None)?;
        let has_more=models::integer(&data,"totalSize").map(|total|page.saturating_mul(PAGE_SIZE)<total.max(0) as u64)
            .unwrap_or(rows.len() as u64==PAGE_SIZE);
        let result=CatalogPage{items,page,has_more,notice:None};
        if self.pages.len()>100{self.pages.clear();}self.pages.insert(cache_key,(Instant::now(),result.clone()));Ok(result)
    }

    fn remember_items(&mut self, rows: &[Value], kind: &str, column_id: Option<i64>, source_page: Option<u64>) -> Result<Vec<Item>,String> {
        let mut result=Vec::new();
        for row in rows {
            if let Some(mut item)=models::normalize_item(row,kind) {
                item.source_category=column_id.map(|column|column.to_string());
                item.source_page=source_page;
                self.items.insert(item_key(kind,&item.id),ItemContext{item:item.clone(),source:row.clone(),column_id,series_content_id:None,episode_number:None});
                result.push(item);
            }
        }
        if !rows.is_empty() && result.is_empty() { return Err("El catálogo recibido no contiene fichas con identificadores válidos.".into()); }
        Ok(result)
    }

    fn details(&mut self, kind: &str, id: &str) -> Result<Item,String> {
        self.ensure_session()?;
        if kind=="live" {
            if let Some(context)=self.items.get(&item_key(kind,id)){return Ok(context.item.clone());}
            if let Some(favorite)=self.favorites.iter().find(|i|i.kind==kind && i.id==id).cloned() {
                if let Some(category)=favorite.source_category {
                    // Refresh only the original page. Do not persist licenses
                    // or crawl the service to restore a saved channel.
                    self.catalog("live",&json!({"category":category,"page":favorite.source_page.unwrap_or(1)}))?;
                    return self.items.get(&item_key(kind,id)).map(|v|v.item.clone())
                        .ok_or_else(||"El canal cambió de posición. Abre su categoría para actualizarlo.".into());
                }
            }
            return Err("Abre primero el canal desde su categoría para obtener sus detalles.".into());
        }
        let context=self.items.get(&item_key(kind,id)).cloned();
        let known_item=context.as_ref().map(|c|c.item.clone()).or_else(||self.favorites.iter().find(|i|i.kind==kind && i.id==id).cloned());
        let source_type=known_item.as_ref().and_then(|item|item.source_type.clone()).unwrap_or_else(||"1".into());
        let column_id=context.as_ref().and_then(|c|c.column_id).or_else(||known_item.as_ref()?.source_category.as_ref()?.parse::<i64>().ok());
        let source_page=known_item.as_ref().and_then(|item|item.source_page);
        let data=self.authenticated_post("K",json!({"contentId":id,"type":source_type,"sortType":"0","language":"en"}))?;
        let asset=data.get("assetData").ok_or_else(|| "La respuesta no incluye la ficha del contenido.".to_string())?;
        let mut item=models::normalize_item(asset,"vod").ok_or_else(|| "La ficha recibida no tiene el formato esperado.".to_string())?;
        // AssetData does not always repeat the type sent in the request.
        if item.source_type.is_none(){item.source_type=Some(source_type);}
        item.source_category=column_id.map(|column|column.to_string());
        item.source_page=source_page;
        if let Some(episodes)=asset.get("simpleProgramList").and_then(Value::as_array) {
            for row in episodes {
                if let Some(episode_id)=models::text(row,"contentId") {
                    let number=models::integer(row,"seriesNumber");
                    let title=models::text(row,"name").unwrap_or_else(|| number.map(|n|format!("Episodio {n}")).unwrap_or_else(||episode_id.clone()));
                    let poster=models::landscape_poster(row).or_else(||row.get("posterList").and_then(Value::as_array).into_iter().flatten()
                        .find_map(|poster| models::text(poster,"fileUrl").and_then(|url|models::http_url(&url))));
                    item.episodes.push(Episode{id:episode_id.clone(),title:title.clone(),number,poster:poster.clone(),duration:models::text(row,"duration")});
                    let episode=Item{id:episode_id.clone(),title,kind:"vod".into(),poster:poster.or_else(||item.poster.clone()),source_type:Some("1".into()),source_category:item.source_category.clone(),source_page,description:None,episodes:vec![],presentation:Default::default()};
                    self.items.insert(item_key("vod",&episode_id),ItemContext{item:episode,source:row.clone(),column_id,series_content_id:Some(id.to_string()),episode_number:number});
                }
            }
        }
        for season in &item.presentation.seasons {
            if season.id != id {
                let mut known=item.clone(); known.id=season.id.clone(); known.episodes.clear(); known.source_category=None;
                self.items.entry(item_key("vod",&season.id)).or_insert(ItemContext { item:known,source:Value::Null,
                    column_id:None,series_content_id:None,episode_number:None });
            }
        }
        self.items.insert(item_key("vod",id),ItemContext{item:item.clone(),source:asset.clone(),column_id,series_content_id:None,episode_number:None});
        Ok(item)
    }

    fn playback(&mut self, kind: &str, id: &str, params: &Value) -> Result<Value,String> {
        playback_preferences(params)?;
        let previous_deadline=self.deadline;
        self.deadline=Some(Instant::now()+Duration::from_secs(45));
        let result=self.playback_inner(kind,id,params);
        self.deadline=previous_deadline;
        result
    }

    fn playback_inner(&mut self, kind:&str, id:&str, params:&Value)->Result<Value,String>{
        let (source,title,selection)=self.resolve_playback_source(kind,id,params)?;
        let deadline=self.deadline.ok_or("La solicitud de reproducción no tiene un plazo válido.")?;
        let mut ticket=register_playback_source(source,deadline,&title)?;
        ticket["selection"]=serialize(selection)?;
        Ok(ticket)
    }

    fn playback_options(&mut self, kind:&str, id:&str, params:&Value)->Result<models::VodPlaybackOptions,String>{
        let (quality,audio_language)=playback_preferences(params)?;
        let previous_deadline=self.deadline;
        self.deadline=Some(Instant::now()+Duration::from_secs(45));
        let result=(||{
            let (data,context)=self.vod_authorization(kind,id)?;
            if !explicit_media_urls(&data,id,context.episode_number).is_empty(){
                if quality.is_some() || audio_language.is_some(){return Err("Esta fuente no ofrece opciones de calidad o idioma.".into());}
                // The restored direct fields do not identify a specific Movie
                // or quality. Do not invent settings or force this path to SLB.
                return Ok(models::VodPlaybackOptions{id:id.into(),kind:"vod".into(),qualities:vec![],selected:None,
                    notice:Some("Este enlace directo no ofrece opciones de calidad o idioma.".into())});
            }
            self.ensure_playback_bootstrap()?;
            let slb=self.slb.as_ref().ok_or("No se ha recibido la configuración del reproductor.")?;
            crate::playback_diagnostics::record_vod_options(&data,slb);
            let (qualities,selection)=models::vod_options_and_selection(&data,slb,id,context.episode_number,quality,audio_language)?;
            Ok(models::VodPlaybackOptions{id:id.into(),kind:"vod".into(),qualities,selected:Some(selection.selected),notice:None})
        })();
        self.deadline=previous_deadline;
        result
    }

    fn vod_authorization(&mut self,kind:&str,id:&str)->Result<(Value,ItemContext),String>{
        self.ensure_session()?;
        let context=match self.items.get(&item_key(kind,id)).cloned() {
            Some(value)=>value,
            None=>{ self.details(kind,id)?; self.items.get(&item_key(kind,id)).cloned().ok_or_else(||"No se pudo localizar el contenido.".to_string())? }
        };
        if kind=="live" { return Err(PLAYBACK_LIMITATION.into()); }
        let key=PlayAuthorizationKey{content_id:id.into(),column_id:context.column_id,
            series_content_id:context.series_content_id.clone(),episode_number:context.episode_number};
        self.play_authorizations.retain(|_,entry|entry.received.elapsed()<PLAY_AUTH_TTL);
        if let Some(entry)=self.play_authorizations.get(&key){return Ok((entry.data.clone(),context));}
        let data=self.authenticated_post("K0",models::vod_start_payload(id,context.column_id,context.series_content_id.as_deref(),context.episode_number))?;
        let bytes=serde_json::to_vec(&data).map_err(|_|"No se pudo preparar la autorización de reproducción.".to_string())?.len();
        // Licenses stay in this process only, with a short TTL and both count
        // and byte limits. Oversized responses can be used without caching.
        if bytes<=4*1024*1024 {
            while self.play_authorizations.len()>=PLAY_AUTH_MAX_ENTRIES
                || self.play_authorizations.values().map(|entry|entry.bytes).sum::<usize>()+bytes>PLAY_AUTH_MAX_BYTES {
                let oldest=self.play_authorizations.iter().min_by_key(|(_,entry)|entry.received).map(|(key,_)|key.clone());
                if let Some(oldest)=oldest{self.play_authorizations.remove(&oldest);}else{break;}
            }
            self.play_authorizations.insert(key,PlayAuthorization{received:Instant::now(),bytes,data:data.clone()});
        }
        Ok((data,context))
    }

    fn subtitle_sources(&mut self, kind:&str, id:&str)->Result<Vec<crate::subtitles::SubtitleSource>,String>{
        if kind == "live" { return Ok(vec![]); }
        let previous_deadline = self.deadline;
        self.deadline = Some(Instant::now() + Duration::from_secs(30));
        let result = (|| {
            let (data, context) = self.vod_authorization(kind,id)?;
            crate::subtitles::sources(&data,id,context.episode_number,None)
        })();
        self.deadline = previous_deadline;
        result
    }

    fn playback_report(&self, params:&Value)->Result<Value,String>{
        let kind = kind(params)?;
        let id = required(params,"id")?;
        let title = self.items.get(&item_key(kind,id)).map(|context|context.item.title.as_str())
            .ok_or("Abre el título antes de guardar su informe.")?;
        let category = required(params,"category")?;
        if !["video","audio","subtitles","quality","cast","other"].contains(&category) {
            return Err("Selecciona el tipo de problema.".into());
        }
        let description = params.get("description").and_then(Value::as_str).unwrap_or("").trim();
        if description.chars().count() > 2000 { return Err("La descripción es demasiado larga.".into()); }
        let supplied = &params["playback"];
        let mut playback = serde_json::Map::new();
        for field in ["current_time","duration"] {
            if let Some(number) = supplied[field].as_f64().filter(|number|number.is_finite() && (0.0..=3_600_000.0).contains(number)) {
                playback.insert(field.into(),json!(number));
            }
        }
        for field in ["video_width","video_height"] {
            if let Some(number) = supplied[field].as_u64().filter(|number|*number<=32768) { playback.insert(field.into(),json!(number)); }
        }
        if let Some(code) = supplied["error_code"].as_u64().filter(|number|*number<=4) { playback.insert("error_code".into(),json!(code)); }
        if let Some(quality) = supplied["quality"].as_str().filter(|quality|["480p","720p","1080p"].contains(quality)) {
            playback.insert("quality".into(),json!(quality));
        }
        if let Some(language) = supplied["audio_language"].as_str().filter(|language|models::valid_audio_language(language)) {
            playback.insert("audio_language".into(),json!(language));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|"El reloj de Windows no es válido.")?.as_secs();
        let reports = self.directory.join("reports");
        fs::create_dir_all(&reports).map_err(|_|"No se pudo crear la carpeta de informes.")?;
        let destination = reports.join(format!("nebula-{now}-{}.json",uuid::Uuid::new_v4().simple()));
        let report = json!({"application":"Nébula","created_at_unix":now,"title":title,"kind":kind,
            "category":category,"description":description,"playback":playback,"sent_to_service":false});
        save_json(&destination,&report)?;
        Ok(json!({"saved":true,"path":destination.to_string_lossy()}))
    }

    // Backend-only source resolution is shared by local playback and Cast.
    // URLs/headers/licenses stay private; only selected labels leave via IPC.
    fn resolve_playback_source(&mut self,kind:&str,id:&str,params:&Value)->Result<(crate::ranger::MediaRequest,String,Option<models::VodSelectedOptions>),String>{
        let (quality,audio_language)=playback_preferences(params)?;
        let (data,context)=self.vod_authorization(kind,id)?;
        if (quality.is_some() || audio_language.is_some()) && !explicit_media_urls(&data,id,context.episode_number).is_empty(){
            return Err("Esta fuente no ofrece opciones de calidad o idioma.".into());
        }
        let mut selected=None;
        let source=prefer_explicit_playback(&data,id,context.episode_number,||{
            let (source,selection)=self.resolve_vod_slb(&data,id,&context,quality,audio_language)?;
            selected=Some(selection);
            Ok(source)
        })?;
        Ok((source,context.item.title,selected))
    }

    fn ensure_playback_bootstrap(&mut self)->Result<(),String>{
        if self.slb.is_none() || self.slb_valid_until.is_none_or(|expiry|expiry<=Instant::now()){
            self.refresh_playback_bootstrap()?;
        }
        Ok(())
    }

    fn resolve_vod_slb(&mut self,data:&Value,id:&str,context:&ItemContext,quality:Option<&str>,audio_language:Option<&str>)->Result<(crate::ranger::MediaRequest,models::VodSelectedOptions),String>{
        self.ensure_playback_bootstrap()?;
        let slb=self.slb.as_ref().ok_or("No se ha recibido la configuración del reproductor.")?;
        crate::playback_diagnostics::record_bootstrap(slb);
        let (_,selection)=models::vod_options_and_selection(data,slb,id,context.episode_number,quality,audio_language)?;
        let selected=selection.selected;
        let selected_media_code=selection.selected_media_code;
        let media:Vec<crate::ranger::RangerMedia>=selection.candidates.into_iter().map(|candidate|crate::ranger::RangerMedia{
            media_code:candidate.media_code,license:candidate.license
        }).collect();
        let mut plans=crate::ranger::prepare_vod_requests(slb,&media)?;
        if self.ranger_identity.is_none() {
            let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map_err(|_|"El reloj de Windows no tiene una fecha válida.")?;
            let random=uuid::Uuid::new_v4();
            let nonce=u32::from_le_bytes(random.as_bytes()[..4].try_into().unwrap());
            // Use the same local Android-ID adaptation already enrolled with
            // the portal. Never import the phone's identity or authorization.
            self.ranger_identity=Some(crate::ranger_identity::generate(
                &self.session.user_id,&self.session.installation_id,now.as_secs(),nonce)?);
        }
        for plan in &mut plans {
            plan.headers.push(("Ranger-Id".into(),self.ranger_identity.as_ref().unwrap().clone()));
        }
        let deadline=self.deadline.ok_or("La solicitud de reproducción no tiene un plazo válido.")?;
        Ok((crate::vod_resolver::resolve(plans,&selected_media_code,deadline)?,selected))
    }

    fn toggle_favorite(&mut self,params:&Value)->Result<Value,String>{
        let kind=kind(params)?;
        let id=required(params,"id")?;
        let mut updated=self.favorites.clone();
        let favorite=if let Some(position)=updated.iter().position(|v|v.id==id && v.kind==kind) {
            updated.remove(position); false
        } else {
            let item=self.items.get(&item_key(kind,id)).map(|v|v.item.clone()).or_else(|| {
                let candidate:Item=serde_json::from_value(params.get("item")?.clone()).ok()?;
                if candidate.id==id && candidate.kind==kind && !candidate.title.trim().is_empty() && candidate.title.len()<=1000 { Some(candidate) } else { None }
            }).ok_or_else(||"No se recibió una ficha válida para guardar en favoritos.".to_string())?;
            updated.push(item); true
        };
        save_json(&self.directory.join("favorites.json"),&updated)?;
        self.favorites=updated;
        Ok(json!({"favorite":favorite,"id":id}))
    }
}

fn register_playback_source(source:crate::ranger::MediaRequest,deadline:Instant,title:&str)->Result<Value,String>{
    let mut headers=reqwest::header::HeaderMap::new();
    for (name,value) in source.headers{
        let name=reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_|"El servidor devolvió una cabecera de vídeo inválida.".to_string())?;
        let value=reqwest::header::HeaderValue::from_str(&value).map_err(|_|"El servidor devolvió una cabecera de vídeo inválida.".to_string())?;
        headers.insert(name,value);
    }
    if MEDIA_PROXY.get().is_none(){
        let proxy=crate::media_proxy::MediaProxy::start(crate::protocol::media_client_builder_until)?;
        let _=MEDIA_PROXY.set(proxy);
    }
    let proxy=MEDIA_PROXY.get().ok_or("No se pudo iniciar el reproductor local.")?;
    let ticket=proxy.register_verified_until(crate::media_proxy::MediaSource{
        url:source.url,kind:crate::media_proxy::MediaKind::File,headers,credential_origins:vec![]
    },deadline)?;
    Ok(json!({"url":ticket.url,"mime_type":ticket.mime_type,"session_id":ticket.session_id,"title":title}))
}

fn prefer_explicit_playback<F>(data:&Value,content_id:&str,episode_number:Option<i64>,slb:F)->Result<crate::ranger::MediaRequest,String>
where F:FnOnce()->Result<crate::ranger::MediaRequest,String>{
    if let Some(url)=explicit_media_urls(data,content_id,episode_number).into_iter().next(){
        return Ok(crate::ranger::MediaRequest{url,headers:vec![]});
    }
    slb()
}

// Restore the previously supported explicit fields, with bounded traversal and
// the selected episode only. An opaque license, generic url, CDN address,
// poster or subtitle is never promoted to a playable URL.
fn explicit_media_urls(data:&Value,content_id:&str,episode_number:Option<i64>)->Vec<reqwest::Url>{
    fn visit(value:&Value,content_id:&str,episode_number:Option<i64>,depth:usize,remaining:&mut usize,urls:&mut Vec<reqwest::Url>){
        if depth>16 || *remaining==0 || urls.len()>=64{return;}
        *remaining-=1;
        match value{
            Value::Object(map)=>{
                for field in ["play_url","playUrl","mediaUrl"]{
                    if urls.len()>=64{break;}
                    if let Some(raw)=map.get(field).and_then(Value::as_str).filter(|url|url.len()<=16_384){
                        if let Some(url)=models::http_url(raw).and_then(|url|reqwest::Url::parse(&url).ok()){
                            if !urls.contains(&url){urls.push(url);}
                        }
                    }
                }
                for (field,nested) in map{
                    if *remaining==0 || urls.len()>=64{break;}
                    if ["play_url","playUrl","mediaUrl","license","licenseList","auth","auths","cdn_list","url_list",
                        "poster","posters","posterList","posterUrl","image","images","thumbnail","thumbnailList","cover","coverList","logo","logos",
                        "subtitle","subtitles","subtitleList"].contains(&field.as_str()){continue;}
                    if field=="episodeList"{
                        if let Some(episodes)=nested.as_array(){
                            let selected=episodes.iter().find(|episode|models::text(episode,"programContentId").as_deref()==Some(content_id))
                                .or_else(||episode_number.and_then(|number|episodes.iter().find(|episode|models::integer(episode,"episodeNumber")==Some(number))))
                                .or_else(||if episode_number.is_none(){episodes.first()}else{None});
                            if let Some(selected)=selected{visit(selected,content_id,episode_number,depth+1,remaining,urls);}
                        }
                    }else{visit(nested,content_id,episode_number,depth+1,remaining,urls);}
                }
            }
            Value::Array(values)=>for nested in values{
                if *remaining==0 || urls.len()>=64{break;}
                visit(nested,content_id,episode_number,depth+1,remaining,urls);
            },
            _=>{}
        }
    }
    let mut urls=Vec::new();
    visit(data,content_id,episode_number,0,&mut 4096,&mut urls);
    urls
}


pub(crate) fn select_vod_server<'a>(resolved:&'a [crate::ranger::ResolvedMedia],authorizations:&[crate::ranger::VodAuthorization],selected_media_code:&str)->Result<&'a crate::ranger::SlbServer,String>{
    // Windows keeps its explicit quality/audio choice: a response for another
    // authorized variant must not silently override the selected Media.
    if !authorizations.iter().any(|auth|auth.media_code==selected_media_code){
        return Err("El servidor no autorizó la versión seleccionada de este título.".into());
    }
    resolved.iter().filter(|media|media.media_code==selected_media_code && media.status_code==200)
        .flat_map(|media|media.servers.iter()).find(|server|!server.media_url.is_empty())
        .ok_or_else(||"El servidor no ofreció una fuente disponible para el vídeo seleccionado.".into())
}

fn item_key(kind:&str,id:&str)->String{format!("{kind}:{id}")}
fn catalog_has_more(kind:&str,data:&Value,page:u64,row_count:usize)->bool{
    // getLiveData reports the current page's count in channelListTotalSize:
    // live integration returned 50 for distinct pages 1 and 2. Shelf totals,
    // by contrast, represent the VOD collection total.
    if kind=="live" { return row_count as u64==PAGE_SIZE; }
    models::integer(data,"assetListTotalSize").map(|total|page*PAGE_SIZE<total.max(0) as u64).unwrap_or(row_count as u64==PAGE_SIZE)
}
fn category_from_row(row:&Value,kind:&str,group:Option<&Category>)->Option<Category>{
    Some(Category{id:models::text(row,"id")?,title:models::text(row,"name").or_else(||models::text(row,"alias"))?,kind:kind.into(),group_id:group.map(|g|g.id.clone()),group_title:group.map(|g|g.title.clone())})
}
fn kind(params:&Value)->Result<&str,String>{
    match params.get("kind").and_then(Value::as_str).unwrap_or("vod") {
        "vod"=>Ok("vod"),"live"=>Ok("live"),_=>Err("Tipo de contenido inválido.".into())
    }
}
fn playback_preferences(params:&Value)->Result<(Option<&str>,Option<&str>),String>{
    fn optional<'a>(params:&'a Value,key:&str)->Result<Option<&'a str>,String>{
        match params.get(key) {
            None|Some(Value::Null)=>Ok(None),
            Some(Value::String(value)) if !value.is_empty()=>Ok(Some(value)),
            _=>Err("La selección de calidad o idioma no es válida.".into()),
        }
    }
    let quality=optional(params,"quality")?;
    let audio_language=optional(params,"audio_language")?;
    if quality.is_some_and(|value|!["480p","720p","1080p"].contains(&value))
        || audio_language.is_some_and(|value|!models::valid_audio_language(value)) {
        return Err("La selección de calidad o idioma no es válida.".into());
    }
    Ok((quality,audio_language))
}
fn filter_request(params:&Value,choices:&Value,page:u64)->Result<Value,String>{
    let filters=params.get("filters").and_then(Value::as_object).ok_or("Selecciona los filtros del catálogo.")?;
    let mapping=[("genre","tags"),("year","year"),("country","originalCountry"),("audio","audio"),("score","score"),("type","programType")];
    if filters.keys().any(|key|!mapping.iter().any(|(name,_)|key==name)){
        return Err("El filtro solicitado no es compatible.".into());
    }
    let mut request=json!({"language":"es","pageNum":page,"pageSize":PAGE_SIZE,
        "originalCountry":"","tags":"","year":"","audio":"","score":"","programType":""});
    for (field,wire) in mapping {
        let value=match filters.get(field){None|Some(Value::Null)=>continue,Some(Value::String(value)) if value.is_empty()=>continue,
            Some(Value::String(value))=>value,_=>return Err("El valor del filtro no es válido.".into())};
        if !choices.get(field).and_then(Value::as_array).is_some_and(|rows|rows.iter().any(|row|row["value"].as_str()==Some(value))){
            return Err("Los filtros disponibles cambiaron. Vuelve a abrir Filtros.".into());
        }
        request[wire]=json!(value);
    }
    if request["programType"]==""{return Err("Selecciona Películas o Series en el filtro Tipo.".into());}
    Ok(request)
}

fn required<'a>(params:&'a Value,key:&str)->Result<&'a str,String>{
    params.get(key).and_then(Value::as_str).filter(|s|!s.is_empty() && s.len()<=1000).ok_or_else(||format!("Falta el parámetro {key}."))
}
fn page(params:&Value)->Result<u64,String>{
    let page=params.get("page").map(|v|v.as_u64().or_else(||v.as_str().and_then(|s|s.parse().ok()))).unwrap_or(Some(1));
    page.filter(|p|(1..=10000).contains(p)).ok_or_else(||"Número de página inválido.".into())
}
fn array<'a>(value:&'a Value,key:&str)->Result<&'a Vec<Value>,String>{
    value.get(key).and_then(Value::as_array).ok_or_else(||format!("El servidor no devolvió la lista {key} con el formato esperado."))
}
fn serialize<T:Serialize>(value:T)->Result<Value,String>{serde_json::to_value(value).map_err(|_|"No se pudo preparar el resultado.".into())}
fn data_directory()->Result<PathBuf,String>{
    if let Some(path)=std::env::var_os("XUPER_DATA_DIR").filter(|p|!p.is_empty()) { return Ok(PathBuf::from(path)); }
    let executable=std::env::current_exe().map_err(|_|"No se pudo ubicar la carpeta de la aplicación.".to_string())?;
    Ok(executable.parent().ok_or_else(||"La ubicación de la aplicación es inválida.".to_string())?.join("data"))
}
fn read_json<T:serde::de::DeserializeOwned>(path:&Path)->Result<T,String>{
    let bytes=fs::read(path).map_err(|_|"No se pudo leer el archivo local de datos.".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_|"El archivo local de datos está dañado; se conservó sin modificarlo.".into())
}
fn save_json<T:Serialize>(path:&Path,value:&T)->Result<(),String>{
    let bytes=serde_json::to_vec_pretty(value).map_err(|_|"No se pudieron preparar los datos locales.".to_string())?;
    let temporary=path.with_extension("tmp");
    fs::write(&temporary,bytes).map_err(|_|"No se pudieron guardar los datos locales.".to_string())?;
    // Same-directory replacement keeps the prior file if replacement fails.
    fs::rename(&temporary,path).map_err(|_|"No se pudo finalizar el guardado local.".into())
}

#[cfg(windows)]
fn physical_memory_bytes()->Option<u64>{
    #[repr(C)]
    struct MemoryStatus{length:u32,load:u32,total_physical:u64,available_physical:u64,total_page_file:u64,available_page_file:u64,total_virtual:u64,available_virtual:u64,available_extended_virtual:u64}
    #[link(name="kernel32")]
    unsafe extern "system"{fn GlobalMemoryStatusEx(buffer:*mut MemoryStatus)->i32;}
    let mut status=MemoryStatus{length:std::mem::size_of::<MemoryStatus>() as u32,load:0,total_physical:0,available_physical:0,total_page_file:0,available_page_file:0,total_virtual:0,available_virtual:0,available_extended_virtual:0};
    if unsafe{GlobalMemoryStatusEx(&mut status)}!=0{Some(status.total_physical)}else{None}
}
#[cfg(not(windows))]
fn physical_memory_bytes()->Option<u64>{None}

#[cfg(test)]
mod tests{
    use super::*;

    struct SessionFixture { directory: PathBuf }
    impl SessionFixture {
        fn new(persisted: bool) -> Self {
            let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis")
                .join(format!("xuper-session-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            if persisted {
                save_json(&directory.join("session.json"), &json!({
                    "installation_id":"0123456789abcdef0123456789abcdef", "sn":"synthetic-sn",
                    "sn_token":"synthetic-sn-token", "user_id":"synthetic-user", "user_token":"synthetic-token",
                    "portal_code":"synthetic-portal", "has_pay":"0", "user_identity":"synthetic-identity"
                })).unwrap();
            }
            let favorite = models::normalize_item(&json!({"contentId":"synthetic-favorite","name":"Saved title"}), "vod").unwrap();
            save_json(&directory.join("favorites.json"), &vec![favorite]).unwrap();
            Self { directory }
        }
        fn api(&self, replies: Vec<(&str, Result<Value, String>)>) -> Api {
            Api::from_directory(self.directory.clone(), Protocol::scripted(replies)).unwrap()
        }
        fn stored_session(&self) -> Value { read_json(&self.directory.join("session.json")).unwrap() }
    }
    impl Drop for SessionFixture {
        fn drop(&mut self) {
            // Only known fixture files are removed, never a recursive target.
            for name in ["session.json", "favorites.json", "session.tmp", "favorites.tmp"] {
                let _ = fs::remove_file(self.directory.join(name));
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
    fn session_reply(data: Value) -> Result<Value, String> { Ok(json!({"returnCode":"0","data":data})) }
    fn replaced_reply() -> Result<Value, String> {
        Ok(json!({"returnCode":"aaa100083","errorMessage":"synthetic server detail must not reach the UI"}))
    }
    fn activated_reply() -> Result<Value, String> {
        session_reply(json!({"userId":"synthetic-user","userToken":"synthetic-refreshed-token",
            "portalCodeList":[{"portalCode":"synthetic-portal"}],"hasPay":"0","userIdentity":"synthetic-identity"}))
    }
    fn called_methods(api: &Api) -> Vec<&str> {
        api.protocol.scripted_calls().iter().map(|(method, _)| method.as_str()).collect()
    }
    #[test]
    fn filters_validate_server_keys_and_keep_wire_values_and_pagination(){
        let choices=json!({"type":[{"value":"movie","label":"Películas"}],"score":[{"value":"8-10","label":"8 o más"}],"genre":[{"value":"drama","label":"Drama"}]});
        let request=filter_request(&json!({"filters":{"type":"movie","score":"8-10","genre":"drama"}}),&choices,2).unwrap();
        assert_eq!(request["score"],"8-10");assert_eq!(request["tags"],"drama");assert_eq!(request["pageNum"],2);
        assert!(filter_request(&json!({"filters":{"score":"injected"}}),&choices,1).is_err());
        assert!(filter_request(&json!({"filters":{"score":8}}),&choices,1).is_err());
        assert!(filter_request(&json!({"filters":{"userToken":"anything"}}),&choices,1).is_err());
    }
    #[test]
    fn filter_options_decode_live_shapes_and_filtered_pages_are_cached(){
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![("H",session_reply(json!({"tags":["Drama"],"year":["2024"],
            "originalCountry":["Argentina"],"audio":[{"key":"spa","value":"Español"}]}))),
            ("z0",session_reply(json!({"totalSize":51,"assetList":[{"contentId":"filtered","name":"Example","score":8.6}]})))]);
        api.initialized=true;
        let choices=api.filter_options().unwrap();assert_eq!(choices["genre"][0]["value"],"Drama");
        assert_eq!(choices["audio"][0]["value"],"spa");assert_eq!(choices["score"][0]["value"],"9");
        let params=json!({"page":1,"filters":{"type":"movie","score":"8","genre":"Drama"}});
        let result=api.filtered_catalog(&params).unwrap();assert_eq!(result.items[0].presentation.score,Some(8.6));assert!(result.has_more);
        api.filtered_catalog(&params).unwrap();assert_eq!(called_methods(&api),vec!["H","z0"]);
        assert_eq!(api.protocol.scripted_calls()[1].1["score"],"8");
        assert_eq!(api.protocol.scripted_calls()[1].1["tags"],"Drama");
        assert!(api.filtered_catalog(&json!({"filters":{"score":"11"}})).is_err());assert_eq!(called_methods(&api),vec!["H","z0"]);
    }
    #[test]
    #[ignore = "Reads the existing desktop session and the live catalog when NEBULA_FILTERS_REAL=1"]
    fn live_filter_contract(){
        assert_eq!(std::env::var("NEBULA_FILTERS_REAL").as_deref(),Ok("1"));
        let mut api=Api::new().unwrap();
        api.ensure_session().unwrap();
        let raw=api.authenticated_post("H",json!({"language":"es"})).unwrap();
        let allowed=["audio","fixAudio","fixCountry","fixGenre","fixRating","fixYear","originalCountry","tags","types","year"];
        let safe:serde_json::Map<String,Value>=allowed.into_iter().filter_map(|key|raw.get(key).cloned().map(|value|(key.into(),value))).collect();
        fs::write(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/filter-options-shape.safe.json"),serde_json::to_vec_pretty(&json!({"fields":raw.as_object().map(|v|v.keys().collect::<Vec<_>>()),"options":safe})).unwrap()).unwrap();
        let choices=api.filter_options().unwrap();
        let selection=choices["score"].as_array().unwrap().iter().find(|row|row["value"]=="8").unwrap();
        let result=api.filtered_catalog(&json!({"page":1,"filters":{"type":"movie","score":selection["value"]}})).unwrap();
        let series=api.filtered_catalog(&json!({"page":1,"filters":{"type":"teleplay","score":selection["value"]}})).unwrap();
        assert!(!series.items.is_empty());assert!(series.items.iter().all(|item|item.source_type.as_deref()==Some("0")&&item.presentation.score.is_some_and(|score|score>=8.0)));
        let series_detail=api.details("vod",&series.items[0].id).unwrap();assert_eq!(series_detail.source_type.as_deref(),Some("0"));assert!(!series_detail.episodes.is_empty());
        let report=json!({"options":choices,"selected_score":selection["value"],"count":result.items.len(),
            "scores":result.items.iter().map(|item|item.presentation.score).collect::<Vec<_>>(),"has_more":result.has_more,
            "series_count":series.items.len(),"series_scores":series.items.iter().map(|item|item.presentation.score).collect::<Vec<_>>(),
            "series_detail_source_type":series_detail.source_type,"series_detail_episode_count":series_detail.episodes.len()});
        fs::write(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/filter-contract-real.safe.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        assert!(!result.items.is_empty());
        assert!(result.items.iter().all(|item|item.presentation.score.is_some_and(|score|score>=8.0)),"The service must honor the numeric minimum score");
    }
    fn seed_playback_context(api:&mut Api,id:&str,episode_number:Option<i64>) {
        let item=models::normalize_item(&json!({"contentId":id,"name":"Synthetic title","type":"1"}),"vod").unwrap();
        api.items.insert(item_key("vod",id),ItemContext{item,source:json!({}),column_id:Some(7),
            series_content_id:episode_number.map(|_|"synthetic-series".into()),episode_number});
    }
    fn playback_options_fixture(id:&str,episode_number:Option<i64>)->Value{
        json!({"episodeList":[{"programContentId":id,"episodeNumber":episode_number.unwrap_or(1),"totalMovieList":[
            {"quality":"480p","movieList":[{"contentId":"private-480-media","audioInfo":"por,eng","licenseList":[{"tag":"allowed","license":"private-license-480"}]}]},
            {"quality":"720p","movieList":[{"contentId":"private-720-media","audioInfo":"spa","licenseList":[{"tag":"allowed","license":"private-license-720"}]}]}
        ]}]})
    }
    fn playback_bootstrap_fixture()->Value{
        json!({"invalidTime":3600,"cdn_list":[{"tag":"vod","serial_number":1,"url_list":[{"tag":"allowed","url":"private-bootstrap-auth"}]}]})
    }

    #[test]
    fn switching_season_keeps_its_type_and_uses_new_parent_for_episode_authorization() {
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![
            ("K",session_reply(json!({"assetData":{"contentId":"season-one","name":"Serie","type":"0",
                "sameSeasonSeriesList":[{"contentId":"season-one","seasonNumber":1},{"contentId":"season-two","seasonNumber":2}],
                "simpleProgramList":[{"contentId":"episode-one","name":"Uno","seriesNumber":1}]}}))),
            ("K",session_reply(json!({"assetData":{"contentId":"season-two","name":"Serie","type":"0",
                "simpleProgramList":[{"contentId":"episode-two","name":"Dos","seriesNumber":1}]}}))),
            ("K0",session_reply(json!({"authorized":"synthetic"}))),
        ]);
        api.initialized=true;
        api.remember_items(&[json!({"contentId":"season-one","name":"Serie","type":"0"})],"vod",Some(7),Some(1)).unwrap();
        let first=api.details("vod","season-one").unwrap();
        assert_eq!(first.presentation.seasons.len(),2);
        let second=api.details("vod","season-two").unwrap();
        assert_eq!(second.episodes[0].id,"episode-two");
        assert_eq!(api.protocol.scripted_calls()[1].1["type"],"0");
        api.vod_authorization("vod","episode-two").unwrap();
        let body=&api.protocol.scripted_calls()[2].1;
        assert_eq!(body["contentId"],"episode-two");
        assert_eq!(body["seriesContentId"],"season-two");
        assert_eq!(body["episodeNumberList"],json!([1]));
        assert_eq!(body["type"],"1");
        assert!(body.get("columnId").is_none());
    }

    #[test]
    fn subtitle_options_reuse_authorization_without_exposing_urls_or_other_episodes() {
        let fixture=SessionFixture::new(true);
        let mut data=playback_options_fixture("episode-two",Some(2));
        data["episodeList"][0]["subtitleList"]=json!([{"language":"es","file":[{"fileType":"srt","url":"https://example.invalid/sub.srt?token=private-subtitle"}]}]);
        data["episodeList"].as_array_mut().unwrap().push(json!({"programContentId":"episode-one","episodeNumber":1,
            "subtitleList":[{"language":"en","file":[{"fileType":"srt","url":"https://example.invalid/other.srt"}]}]}));
        let mut api=fixture.api(vec![("K0",session_reply(data))]);
        api.initialized=true;
        seed_playback_context(&mut api,"episode-two",Some(2));
        let first=api.subtitle_sources("vod","episode-two").unwrap();
        let next=api.subtitle_sources("vod","episode-two").unwrap();
        assert_eq!(called_methods(&api),vec!["K0"]);
        assert_eq!(first.len(),1);
        assert_eq!(first[0].track.language,"es");
        assert_eq!(first[0].track.id,next[0].track.id);
        let exposed=serde_json::to_string(&first[0].track).unwrap();
        assert!(!exposed.contains("private-subtitle") && !exposed.contains("http"));
    }

    #[test]
    fn local_problem_report_keeps_only_requested_noncredential_fields() {
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![]);
        seed_playback_context(&mut api,"episode-two",Some(2));
        let response=api.playback_report(&json!({"kind":"vod","id":"episode-two","category":"subtitles","description":"Una línea no aparece.",
            "playback":{"current_time":12.5,"video_width":854,"quality":"480p","audio_language":"kor",
                "url":"https://example.invalid/private-token","license":"private-license","error_code":999}})).unwrap();
        let path=PathBuf::from(response["path"].as_str().unwrap());
        let report:Value=read_json(&path).unwrap();
        assert_eq!(report["description"],"Una línea no aparece.");
        assert_eq!(report["playback"]["current_time"],12.5);
        assert!(report["playback"].get("error_code").is_none());
        assert_eq!(report["sent_to_service"],false);
        assert!(!report.to_string().contains("private-"));
        assert!(called_methods(&api).is_empty());
        fs::remove_file(&path).unwrap();
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn playback_options_reuse_k0_for_selection_and_playback_preparation_without_leaks(){
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![("K0",session_reply(playback_options_fixture("episode-one",Some(1)))),
            ("y",session_reply(playback_bootstrap_fixture()))]);
        api.initialized=true;
        seed_playback_context(&mut api,"episode-one",Some(1));
        let first=serialize(api.playback_options("vod","episode-one",&json!({})).unwrap()).unwrap();
        assert_eq!(first["selected"]["quality"],"480p");
        assert_eq!(first["selected"]["audio_language"],"por");
        let changed=serialize(api.playback_options("vod","episode-one",&json!({"quality":"720p","audio_language":"spa"})).unwrap()).unwrap();
        assert_eq!(changed["selected"]["quality"],"720p");
        assert_eq!(changed["selected"]["audio_language"],"spa");
        assert_eq!(changed["selected"]["audio_track_index"],0);
        // Playback consumes exactly this same authorization helper. An invalid
        // explicit combination fails before any SLB request or media probe.
        let error=api.resolve_playback_source("vod","episode-one",&json!({"quality":"480p","audio_language":"spa"})).err().unwrap();
        assert!(error.contains("idioma"));
        assert_eq!(called_methods(&api),vec!["K0","y"]);
        assert_eq!(api.protocol.scripted_calls()[0].1["seriesContentId"],"synthetic-series");
        assert_eq!(api.protocol.scripted_calls()[0].1["episodeNumberList"],json!([1]));
        assert!(api.deadline.is_none());
        for exposed in [first,changed] {
            let wire=exposed.to_string();
            for private in ["private-","license","userToken","media_code","url_list"] {assert!(!wire.contains(private));}
        }
    }

    #[test]
    fn playback_authorization_cache_tracks_context_ttl_capacity_and_session(){
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api((0..13).map(|_|("K0",session_reply(json!({"authorized":"synthetic"})))).collect());
        api.initialized=true;
        seed_playback_context(&mut api,"same-id",Some(1));
        api.vod_authorization("vod","same-id").unwrap();
        api.vod_authorization("vod","same-id").unwrap();
        assert_eq!(called_methods(&api),vec!["K0"]);
        api.items.get_mut(&item_key("vod","same-id")).unwrap().episode_number=Some(2);
        api.vod_authorization("vod","same-id").unwrap();
        assert_eq!(api.protocol.scripted_calls()[1].1["episodeNumberList"],json!([2]));
        for entry in api.play_authorizations.values_mut(){entry.received=Instant::now()-PLAY_AUTH_TTL-Duration::from_secs(1);}
        api.vod_authorization("vod","same-id").unwrap();
        assert_eq!(called_methods(&api).len(),3);
        for index in 0..10 {
            let id=format!("different-{index}");
            seed_playback_context(&mut api,&id,None);
            api.vod_authorization("vod",&id).unwrap();
        }
        assert_eq!(api.play_authorizations.len(),PLAY_AUTH_MAX_ENTRIES);
        assert!(api.play_authorizations.values().map(|entry|entry.bytes).sum::<usize>()<=PLAY_AUTH_MAX_BYTES);
        api.invalidate_replaced_session();
        assert!(api.play_authorizations.is_empty());
        assert!(is_session_expired_error(&api.vod_authorization("vod","same-id").err().unwrap()));
        assert_eq!(called_methods(&api).len(),13);
    }

    #[test]
    fn playback_options_preserve_opaque_direct_source_without_inventing_settings(){
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![("K0",session_reply(json!({"play_url":"https://example.invalid/private-direct.mp4"})))]);
        api.initialized=true;
        seed_playback_context(&mut api,"direct-title",None);
        let options=serialize(api.playback_options("vod","direct-title",&json!({})).unwrap()).unwrap();
        assert_eq!(options["qualities"],json!([]));
        assert!(options["selected"].is_null());
        assert!(!options.to_string().contains("private-direct"));
        let (source,_,selection)=api.resolve_playback_source("vod","direct-title",&json!({})).unwrap();
        assert_eq!(source.url.as_str(),"https://example.invalid/private-direct.mp4");
        assert!(selection.is_none());
        assert!(api.resolve_playback_source("vod","direct-title",&json!({"quality":"720p"})).is_err());
        assert_eq!(called_methods(&api),vec!["K0"]);
        assert!(api.slb.is_none());
    }

    #[test]
    fn malformed_playback_preferences_are_rejected_before_session_or_network(){
        let fixture=SessionFixture::new(true);
        let mut api=fixture.api(vec![]);
        for params in [json!({"quality":"2160p"}),json!({"quality":7}),json!({"audio_language":"por,eng"}),json!({"audio_language":"https://secret.invalid"}),json!({"audio_language":""})] {
            assert!(api.playback_options("vod","synthetic-title",&params).is_err());
            assert!(api.playback("vod","synthetic-title",&params).is_err());
        }
        assert!(called_methods(&api).is_empty());
        assert!(!api.initialized && api.deadline.is_none());
    }

    #[test]
    fn persisted_session_is_validated_without_activation() {
        let fixture = SessionFixture::new(true);
        let before = fixture.stored_session();
        let mut api = fixture.api(vec![("e0", session_reply(json!({})))]);
        assert_eq!(api.status().unwrap()["connected"], true);
        assert_eq!(api.status().unwrap()["connected"], true);
        assert_eq!(called_methods(&api), vec!["e0"]);
        assert_eq!(fixture.stored_session(), before);
        assert_eq!(api.protocol.scripted_calls()[0].1["userToken"], "synthetic-token");
        assert_eq!(api.favorites[0].id, "synthetic-favorite");
    }

    #[test]
    fn replaced_session_discards_cached_success_and_blocks_automatic_activation() {
        let fixture = SessionFixture::new(true);
        let before = fixture.stored_session();
        let mut api = fixture.api(vec![("e0", session_reply(json!({}))), ("Z", replaced_reply())]);
        api.ensure_session().unwrap();
        let now = Instant::now();
        api.categories.insert("vod".into(), (now, vec![Category { id:"7".into(), title:"Cached".into(), kind:"vod".into(), group_id:None, group_title:None }]));
        let item = api.favorites[0].clone();
        api.pages.insert("vod:7:1".into(), (now, CatalogPage { items:vec![item.clone()], page:1, has_more:false, notice:None }));
        api.items.insert(item_key("vod", &item.id), ItemContext { item, source:json!({}), column_id:Some(7), series_content_id:None, episode_number:None });
        api.slb = Some(json!({"cdn_list":[]}));
        api.slb_valid_until = Some(now + Duration::from_secs(300));
        let error = api.authenticated_post("Z", json!({})).unwrap_err();
        assert!(is_session_expired_error(&error));
        assert!(!api.initialized);
        assert!(api.categories.is_empty() && api.pages.is_empty() && api.items.is_empty());
        assert!(api.slb.is_none() && api.slb_valid_until.is_none());
        let status = api.status().unwrap();
        assert_eq!(status["connected"], false);
        assert_eq!(status["error_code"], "session_expired");
        assert!(is_session_expired_error(&api.categories("vod").err().unwrap()));
        assert!(is_session_expired_error(&api.catalog("vod", &json!({"category":"7"})).err().unwrap()));
        // The rejection remains sticky even after the general error cooldown.
        api.boot_error = Some((now - Duration::from_secs(30), "synthetic old error".into()));
        assert!(is_session_expired_error(&api.ensure_session().unwrap_err()));
        assert_eq!(called_methods(&api), vec!["e0", "Z"]);
        assert_eq!(fixture.stored_session(), before);
        assert_eq!(api.favorites[0].id, "synthetic-favorite");
    }

    #[test]
    fn explicit_reconnect_activates_once_preserves_identity_and_recovers() {
        let fixture = SessionFixture::new(true);
        let before = fixture.stored_session();
        let mut api = fixture.api(vec![("e0", replaced_reply()), ("D", activated_reply()),
            ("e0", session_reply(json!({}))), ("Z", session_reply(json!({"childColumnList":[]})))]);
        assert_eq!(api.status().unwrap()["connected"], false);
        assert_eq!(api.reconnect_session().unwrap()["connected"], true);
        // API commands are serialized: an already queued second click reaches
        // this state and must not replace the just-issued token again.
        assert_eq!(api.reconnect_session().unwrap()["connected"], true);
        assert_eq!(api.authenticated_post("Z", json!({})).unwrap()["childColumnList"], json!([]));
        assert_eq!(called_methods(&api), vec!["e0", "D", "e0", "Z"]);
        let after = fixture.stored_session();
        for key in ["installation_id", "sn", "sn_token", "user_id", "portal_code"] {
            assert_eq!(after[key], before[key]);
        }
        assert_eq!(after["user_token"], "synthetic-refreshed-token");
        assert_eq!(api.protocol.scripted_calls()[1].1["sn"], "synthetic-sn");
        assert_eq!(api.protocol.scripted_calls()[1].1["snToken"], "synthetic-sn-token");
        assert_eq!(api.protocol.scripted_calls()[3].1["userToken"], "synthetic-refreshed-token");
        let favorites: Vec<Item> = read_json(&fixture.directory.join("favorites.json")).unwrap();
        assert_eq!(favorites[0].id, "synthetic-favorite");
    }

    #[test]
    fn failed_reconnect_is_not_repeated_by_queued_commands() {
        let fixture = SessionFixture::new(true);
        let before = fixture.stored_session();
        let mut api = fixture.api(vec![("e0", replaced_reply()), ("D", Err("Synthetic transport timeout".into()))]);
        assert_eq!(api.status().unwrap()["connected"], false);
        assert_eq!(api.reconnect_session().unwrap_err(), "Synthetic transport timeout");
        assert_eq!(api.reconnect_session().unwrap_err(), "Synthetic transport timeout");
        assert_eq!(api.status().unwrap()["connected"], false);
        assert!(is_session_expired_error(&api.ensure_session().unwrap_err()));
        assert_eq!(called_methods(&api), vec!["e0", "D"]);
        assert_eq!(fixture.stored_session(), before);
    }

    #[test]
    fn fresh_installation_keeps_normal_enrollment() {
        let fixture = SessionFixture::new(false);
        let mut api = fixture.api(vec![("o", session_reply(json!({"snToken":"synthetic-sn-token","isNew":"1","sn":"synthetic-sn"}))),
            ("D", activated_reply()), ("e0", session_reply(json!({})))]);
        let installation = api.session.installation_id.clone();
        assert_eq!(api.status().unwrap()["connected"], true);
        assert_eq!(called_methods(&api), vec!["o", "D", "e0"]);
        assert_eq!(api.session.installation_id, installation);
        assert_eq!(api.session.sn, "synthetic-sn");
        assert_eq!(api.favorites[0].id, "synthetic-favorite");
    }

    fn diagnostic_config_shape(value:&Value,depth:usize)->Value {
        if depth>12{return json!("depth limit");}
        match value {
            Value::Object(fields)=>Value::Object(fields.iter().map(|(key,value)|(key.clone(),diagnostic_config_shape(value,depth+1))).collect()),
            Value::Array(values)=>json!({"type":"array","count":values.len(),"first":values.first().map(|value|diagnostic_config_shape(value,depth+1))}),
            Value::String(text)=>{
                if let Ok(parsed)=serde_json::from_str::<Value>(text) {
                    if parsed.is_object() || parsed.is_array(){return json!({"type":"JSON string","shape":diagnostic_config_shape(&parsed,depth+1)});}
                }
                let mut keys=Vec::new();
                for part in text.split(['&',';','\n','\r',',']) {
                    if let Some((key,_))=part.split_once('=').or_else(||part.split_once(':')) {
                        let key=key.trim().trim_start_matches('?');
                        if key.chars().next().is_some_and(|c|c.is_ascii_alphabetic() || c=='_') && key.len()<=64
                            && key.chars().all(|c|c.is_ascii_alphanumeric() || matches!(c,'_'|'-')) && !keys.contains(&key) {keys.push(key);}
                    }
                }
                json!({"type":"string","length":text.len(),"parameter_keys":keys,"has_line_breaks":text.contains(['\r','\n'])})
            },
            Value::Number(_)=>json!("number"),Value::Bool(_)=>json!("boolean"),Value::Null=>Value::Null,
        }
    }
    #[test]
    fn stopping_playback_does_not_wait_for_the_api_mutex(){
        let guard=API.get_or_init(||Mutex::new(None)).lock().unwrap();
        let (sender,receiver)=std::sync::mpsc::channel();
        let worker=std::thread::spawn(move||{
            let result=handle("playback.stop",&json!({"session_id":"nonexistent-local-test-session"}));
            let _=sender.send(result);
        });
        let stopped=receiver.recv_timeout(Duration::from_millis(250)).expect("Stop must bypass the locked API").unwrap();
        assert_eq!(stopped["stopped"],false);
        drop(guard);
        worker.join().unwrap();
    }
    /// Explicit diagnostic of the already-issued local session. Never run as
    /// part of normal unit tests; no new installation or activation is made.
    #[test]
    #[ignore]
    fn diagnose_authorized_catalog(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("catalog"),"Explicit catalog diagnostic flag required");
        let directory=data_directory().unwrap();
        assert!(directory.join("session.json").exists(),"An existing own session is required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"An existing authorized session is required");
        api.initialized=true;
        fn summary(value:&Value)->Value {
            let mut result=serde_json::Map::new();
            if let Some(object)=value.as_object(){
                for (key,value) in object {
                    result.insert(key.clone(),match value {
                        Value::Array(rows)=>json!({"count":rows.len(),"first_fields":rows.first().and_then(Value::as_object).map(|o|o.keys().cloned().collect::<Vec<_>>())}),
                        Value::Object(fields)=>json!({"fields":fields.keys().cloned().collect::<Vec<_>>()}),
                        Value::Number(number)=>json!(number),
                        Value::Bool(boolean)=>json!(boolean),
                        Value::Null=>Value::Null,
                        _=>json!("string omitted"),
                    });
                }
            }
            Value::Object(result)
        }
        fn column(row:&Value)->Value {
            let mut result=serde_json::Map::new();
            for key in ["id","name","alias","type","columnType","specialFlag"] {
                if let Some(value)=row.get(key){result.insert(key.into(),value.clone());}
            }
            Value::Object(result)
        }
        let categories=api.categories("vod").unwrap();
        let mut results=Vec::new();
        for category in categories.iter().take(6) {
            let id=category.id.parse::<i64>().unwrap();
            let children=api.authenticated_post("Z",json!({"columnId":id,"specialFlag":"0","pageNum":1,"pageSize":1000})).unwrap();
            let shelves=api.authenticated_post("x0",json!({"columnId":id,"columnType":"2","pageNum":1,"pageSize":50})).unwrap();
            let child_rows=array(&children,"childColumnList").unwrap();
            let mut result=json!({"category":category,"columns":child_rows.iter().take(30).map(column).collect::<Vec<_>>(),"children_response":summary(&children),"shelf_response":summary(&shelves)});
            if let Some(child)=child_rows.first().and_then(|row|models::integer(row,"id")) {
                let nested=api.authenticated_post("x0",json!({"columnId":child,"columnType":"2","pageNum":1,"pageSize":50})).unwrap();
                result["first_child_id"]=json!(child);
                result["first_child_shelf"]=summary(&nested);
            }
            results.push(result);
        }
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/catalog-diagnostic.safe.json"),&results).unwrap();
        println!("Catalog diagnostic complete; {} category summaries saved without session fields.",results.len());
    }
    #[test]
    #[ignore]
    fn diagnose_authorized_read_paths(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("read_paths"),"Explicit read-path diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        api.initialized=true;
        let vod=api.categories("vod").unwrap();
        let first=vod.first().unwrap();
        let catalog=api.catalog("vod",&json!({"category":first.id,"page":1})).unwrap();
        let mut report=json!({"vod_categories":vod.len(),"first_vod_category":first,"vod_items":catalog.items.len(),"vod_has_more":catalog.has_more});
        if let Some(item)=catalog.items.first() {
            match api.details("vod",&item.id) {
                Ok(details)=>report["details"]=json!({"valid":true,"description_present":details.description.is_some(),"episode_count":details.episodes.len(),"source_type":details.source_type}),
                Err(error)=>report["details"]=json!({"error":error}),
            }
            match api.search("vod",&json!({"query":item.title,"page":1})) {
                Ok(search)=>report["search"]=json!({"items":search.items.len(),"has_more":search.has_more}),
                Err(error)=>report["search"]=json!({"error":error}),
            }
        }
        let live=api.categories("live").unwrap();
        report["live_categories"]=json!(live.len());
        if let Some(first)=live.first() {
            report["first_live_category"]=json!(first);
            match api.catalog("live",&json!({"category":first.id,"page":1})) {
                Ok(page)=>report["live"]=json!({"items":page.items.len(),"has_more":page.has_more}),
                Err(error)=>report["live"]=json!({"error":error}),
            }
        }
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/read-paths-diagnostic.safe.json"),&report).unwrap();
        println!("Read-path diagnostic complete; safe counts saved.");
    }
    #[test]
    #[ignore]
    fn diagnose_authorized_live_shape(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("live_shape"),"Explicit live-shape diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        api.initialized=true;
        let categories=api.categories("live").unwrap();
        let id=categories.first().unwrap().id.parse::<i64>().unwrap();
        let mut reports=Vec::new();
        let mut first_ids=Vec::new();
        for page in [1,2] {
            let data=api.authenticated_post("i",json!({"columnId":id,"dataVersion":"","expireTimeStr":"","pageNum":page,"pageSize":PAGE_SIZE})).unwrap();
            let mut shape=serde_json::Map::new();
            for (key,value) in data.as_object().unwrap() {
                shape.insert(key.clone(),match value {
                    Value::Number(number)=>json!(number),
                    Value::Array(values)=>json!({"count":values.len(),"first_fields":values.first().and_then(Value::as_object).map(|o|o.keys().cloned().collect::<Vec<_>>())}),
                    _=>json!("value omitted"),
                });
            }
            let first=data.get("channelList").and_then(Value::as_array).and_then(|a|a.first());
            first_ids.push(first.and_then(|v|models::text(v,"channelCode")));
            if let Some(first)=first {
                shape.insert("live_address_shape".into(),json!(first.get("liveAddressList").and_then(Value::as_array).and_then(|a|a.first()).and_then(Value::as_object).map(|o|o.keys().cloned().collect::<Vec<_>>())));
            }
            reports.push(json!({"page":page,"data":shape}));
        }
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/live-shape-diagnostic.safe.json"),&json!({"pages":reports,"page_first_id_differs":first_ids[0]!=first_ids[1]})).unwrap();
        println!("Live shape diagnostic complete; only structure/counts saved.");
    }
    #[test]
    #[ignore]
    fn diagnose_vod_quality_options(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("vod_quality"),"Explicit quality diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        // Read the existing session without enrollment, activation or token replacement.
        api.initialized=true;
        let mut reports=Vec::new();
        let categories=api.categories("vod").unwrap();
        if let Some(category)=categories.iter().find(|category|category.title.to_lowercase().contains("criminal")) {
            let catalog=api.catalog("vod",&json!({"category":category.id,"page":1})).unwrap();
            if let Some(item)=catalog.items.first() {
                api.details("vod",&item.id).unwrap();
                let (play,context)=api.vod_authorization("vod",&item.id).unwrap();
                let episode=models::vod_episode(&play,&item.id,context.episode_number).unwrap();
                let groups:Vec<_>=episode["totalMovieList"].as_array().into_iter().flatten().map(|group|json!({
                    "quality":group["quality"].as_str().filter(|q|["480p","720p","1080p","2160p"].contains(q)),
                    "movie_count":group["movieList"].as_array().map_or(0,Vec::len)
                })).collect();
                let options=api.playback_options("vod",&item.id,&json!({})).unwrap();
                reports.push(json!({"title":item.title,"groups":groups,"options":options.qualities,
                    "selected":options.selected,"has_column_context":context.column_id.is_some()}));
                if std::env::var("XUPER_DIAGNOSTIC_MEDIA").as_deref()==Ok("1") {
                    let result=api.playback("vod",&item.id,&json!({"quality":"1080p"}));
                    let summary=match result {
                        Ok(ticket)=>{if let Some(id)=ticket["session_id"].as_str(){if let Some(proxy)=MEDIA_PROXY.get(){proxy.release(id);}}
                            json!({"stream_verified":true,"mime_type":ticket["mime_type"],"selected":ticket["selection"]})},
                        Err(error)=>json!({"stream_verified":false,"message":error})
                    };
                    save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/quality-stream-probe.safe.json"),&summary).unwrap();
                }
            }
        }
        for query in ["El irlandés","Hope","Enemigos públicos"] {
            let catalog=api.search("vod",&json!({"query":query,"page":1})).unwrap();
            for item in catalog.items.into_iter().take(1) {
                api.details("vod",&item.id).unwrap();
                let (play,context)=api.vod_authorization("vod",&item.id).unwrap();
                let episode=models::vod_episode(&play,&item.id,context.episode_number).unwrap();
                let groups:Vec<_>=episode["totalMovieList"].as_array().into_iter().flatten().map(|group|json!({
                    "quality":group["quality"].as_str().filter(|q|["480p","720p","1080p","2160p"].contains(q)),
                    "movie_count":group["movieList"].as_array().map_or(0,Vec::len)
                })).collect();
                let options=api.playback_options("vod",&item.id,&json!({})).unwrap();
                reports.push(json!({"title":item.title,"groups":groups,"options":options.qualities,
                    "selected":options.selected,"has_column_context":context.column_id.is_some()}));
            }
        }
        let report=json!({"source_api_version":api.protocol.api_version(),"session_reactivated":false,"private_values_exported":false,"titles":reports});
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/quality-options-real.safe.json"),&report).unwrap();
        println!("Quality metadata comparison saved; no licenses, tokens or media URLs exported.");
    }
    #[test]
    #[ignore]
    fn diagnose_vod_higher_resolutions(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("vod_resolutions"),"Explicit resolution diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        api.initialized=true;
        let mut seen=std::collections::HashSet::new();let mut qualities=std::collections::BTreeSet::new();
        let mut reports=Vec::new();let mut failed_queries=Vec::new();
        for query in ["Avatar","Dune","Interstellar","Oppenheimer","The Batman","Top Gun","El señor de los anillos","Avengers","Matrix","Spider-Man","Gladiador","Misión imposible"] {
            let catalog=match api.search("vod",&json!({"query":query,"page":1})) {Ok(value)=>value,Err(_)=>{failed_queries.push(query);continue;}};
            for item in catalog.items.into_iter().take(2) {
                if !seen.insert(item.id.clone()){continue;}
                if api.details("vod",&item.id).is_err(){continue;}
                let Ok((play,context))=api.vod_authorization("vod",&item.id)else{continue;};
                let Ok(episode)=models::vod_episode(&play,&item.id,context.episode_number)else{continue;};
                let groups:Vec<_>=episode["totalMovieList"].as_array().into_iter().flatten().filter_map(|group| {
                    let name=group["quality"].as_str()?.chars().take(80).collect::<String>();qualities.insert(name.clone());
                    let movies:Vec<_>=group["movieList"].as_array().into_iter().flatten().map(|movie|json!({
                        "container":models::text(movie,"videoFormat"),"codec":models::text(movie,"encodeFormat")
                    })).collect();
                    Some(json!({"quality":name,"movie_count":movies.len(),"formats":movies}))
                }).collect();
                reports.push(json!({"title":item.title,"groups":groups}));
            }
        }
        let report=json!({"session_reactivated":false,"private_values_exported":false,"media_downloads":0,
            "unique_quality_labels":qualities,"titles_checked":reports.len(),"failed_queries":failed_queries,"titles":reports});
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/higher-resolutions-real.safe.json"),&report).unwrap();
        println!("Resolution metadata sample saved; no media files downloaded.");
    }
    #[test]
    #[ignore]
    fn diagnose_authorized_vod_slb(){
        use std::io::Read;
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("vod_slb"),"Explicit SLB diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        api.initialized=true;
        let analysis=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis");
        let catalog_evidence:Value=read_json(&analysis.join("catalog-diagnostic.safe.json")).unwrap();
        let free=catalog_evidence.as_array().unwrap().iter().flat_map(|group|group["columns"].as_array().into_iter().flatten())
            .find(|column|column["name"].as_str()==Some("Free")).and_then(|column|models::integer(column,"id")).expect("Previously observed Free shelf required");
        let catalog=api.catalog("vod",&json!({"category":free.to_string(),"page":1})).unwrap();
        let item=catalog.items.first().expect("Free shelf contains a selectable title");
        let play=api.authenticated_post("K0",models::vod_start_payload(&item.id,Some(free),None,None)).unwrap();
        let explicit_media_url_count=explicit_media_urls(&play,&item.id,None).len();
        if explicit_media_url_count>0{
            save_json(&analysis.join("vod-slb-diagnostic.safe.json"),&json!({
                "route":"explicit","explicit_media_url_present":true,"explicit_media_url_count":explicit_media_url_count,
                "explicit_media_url_count_limit":64,"slb_selected":false,"slb_requests":0,"media_fetches":0,
                "note":"A direct media source was found; this diagnostic does not fetch media."
            })).unwrap();
            println!("VOD source diagnostic complete; explicit source selected, no SLB or media fetched.");
            return;
        }
        let slb=api.authenticated_post("y",json!({"encMediaSupported":1,"liveCodeList":[format!("{}_live",api.session.portal_code)],
            "type":"merge","appVer":api.protocol.api_version().to_string(),"lang":"pt","reserve1":"","pipFlag":"0",
            "hasPay":api.session.has_pay,"userIdentity":api.session.user_identity})).unwrap();
        let media:Vec<crate::ranger::RangerMedia>=models::vod_candidates(&play,&slb,&item.id,None).unwrap().into_iter()
            .map(|candidate|crate::ranger::RangerMedia{media_code:candidate.media_code,license:candidate.license}).collect();
        let plans=crate::ranger::prepare_vod_requests(&slb,&media).unwrap();
        let plan=plans.first().expect("Matching SLB plan required");
        // Offline comparison only: the previous planner admitted every
        // business except live. Re-labeling those entries reproduces its
        // candidate set through the corrected VOD-only planner, without
        // sending any request for the old set or recording private URLs.
        let mut previous_slb=slb.clone();
        if let Some(entries)=previous_slb.get_mut("cdn_list").and_then(Value::as_array_mut){
            for entry in entries{
                if entry.get("tag").and_then(Value::as_str)!=Some("live"){entry["tag"]=json!("vod");}
            }
        }
        let previous_plans=crate::ranger::prepare_vod_requests(&previous_slb,&media).unwrap_or_default();
        let selected_business=slb.get("cdn_list").and_then(Value::as_array).into_iter().flatten().find_map(|entry|{
            if entry.get("tag").and_then(Value::as_str)!=Some("vod") || entry.get("cdn_id_mark").and_then(Value::as_str).unwrap_or("")!=plan.entry_name{return None;}
            let same_origin=["main_addr","spared_addr"].iter().filter_map(|field|entry.get(*field).and_then(Value::as_str)).any(|address|{
                let address=if address.contains("://"){address.to_string()}else{format!("https://{address}")};
                reqwest::Url::parse(&address).is_ok_and(|url|url.origin()==plan.url.origin())
            });
            if same_origin{Some("vod")}else{None}
        }).unwrap_or("other");
        assert_eq!(selected_business,"vod","Selected plan must trace to the server's VOD business");
        let (builder,_)=crate::protocol::media_client_builder(&plan.url).unwrap();
        let client=builder.tls_info(true).build().unwrap();
        let mut request=client.get(plan.url.clone());
        for (name,value) in &plan.headers { request=request.header(name,value); }
        let response=request.send().map_err(|_|"SLB connection failed; URL omitted").unwrap();
        let http_status=response.status().as_u16();
        let http_version=match response.version(){reqwest::Version::HTTP_10=>"HTTP/1.0",reqwest::Version::HTTP_11=>"HTTP/1.1",reqwest::Version::HTTP_2=>"HTTP/2",_=>"other"};
        let peer_certificate_present=response.extensions().get::<reqwest::tls::TlsInfo>().and_then(|tls|tls.peer_certificate()).is_some();
        let server_family=response.headers().get(reqwest::header::SERVER).and_then(|value|value.to_str().ok()).map(|value|{
            let value=value.to_ascii_lowercase();
            ["nginx","cloudflare","apache","envoy","akamai"].into_iter().find(|family|value.starts_with(family)).unwrap_or("other")
        });
        let content_type=response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|header|header.to_str().ok()).map(|value|value.to_string());
        let mut body=String::new();
        response.take(65_537).read_to_string(&mut body).map_err(|_|"SLB body read failed; body omitted").unwrap();
        fn url_shape(raw:&str)->Value {
            if raw.is_empty(){return Value::Null;}
            match reqwest::Url::parse(raw) {
                Ok(url)=>{
                    let segments:Vec<String>=url.path().split('/').map(|part|{
                        if part.is_empty(){return String::new();}
                        let suffix=[".m3u8",".mp4",".mpd",".ts",".m4s"].iter().find(|suffix|part.ends_with(**suffix)).copied().unwrap_or("");
                        format!("{{segment}}{suffix}")
                    }).collect();
                    json!({"scheme":url.scheme(),"path":segments.join("/"),"query_keys":url.query_pairs().map(|(key,_)|key.to_string()).collect::<Vec<_>>(),"ends_with_slash":url.path().ends_with('/')})
                },
                Err(_)=>json!({"parseable_url":false,"length":raw.len()}),
            }
        }
        fn server_shape(server:&crate::ranger::SlbServer)->Value {
            json!({"tag_present":!server.tag.is_empty(),"server_code_present":!server.server_code.is_empty(),"media_url":url_shape(&server.media_url),"idx_url":url_shape(&server.idx_url),"snapshot_url":url_shape(&server.snapshot_url),"torrent_url":url_shape(&server.torrent_url)})
        }
        let entry_shapes:Vec<Value>=slb.get("cdn_list").and_then(Value::as_array).into_iter().flatten().map(|entry|{
            let mut fields=serde_json::Map::new();
            for key in ["redirect","cdn_type"] {if let Some(value)=entry.get(key){fields.insert(key.into(),value.clone());}}
            fields.insert("business".into(),json!(match entry.get("tag").and_then(Value::as_str){Some("vod")=>"vod",Some("live")=>"live",Some("record")=>"record",_=>"other"}));
            Value::Object(fields)
        }).collect();
        let mut report=json!({"route":"slb","explicit_media_url_present":false,"explicit_media_url_count":explicit_media_url_count,"explicit_media_url_count_limit":64,"slb_selected":true,"slb_requests":1,
            "http_status":http_status,"http_version":http_version,"server_family":server_family,"tls_validation":"Windows default certificate and hostname validation","peer_certificate_present":peer_certificate_present,
            "response_content_type":content_type,"request_method":plan.method(),"request_path":"/slb/v11/vod",
            "request_scheme":plan.url.scheme(),"request_port":plan.url.port_or_known_default(),"request_query_length":plan.url.query().map(str::len),
            "candidate_media_count":media.len(),"plan_count":plans.len(),"previous_algorithm_plan_count":previous_plans.len(),
            "first_plan_changed_from_previous_algorithm":previous_plans.first().map(|previous|previous.url!=plan.url).unwrap_or(true),
            "selected_entry_business":selected_business,"response_bytes":body.len(),"media_fetches":0,"entry_shapes":entry_shapes,
            "response_is_html":body.trim_start().starts_with('<'),"response_mentions_nginx":body.to_ascii_lowercase().contains("nginx"),
            "response_plain_http_to_https_error":body.contains("plain HTTP request was sent to HTTPS port"),
            "response_header_too_large_error":body.contains("Request Header Or Cookie Too Large"),
            "response_json_fields":serde_json::from_str::<Value>(&body).ok().and_then(|value|value.as_object().map(|object|object.keys().cloned().collect::<Vec<_>>()))});
        report["play_params_shape"]=slb.get("play_params").map(|value|diagnostic_config_shape(value,0)).unwrap_or(Value::Null);
        report["gslb_params_shapes"]=json!(slb.get("cdn_list").and_then(Value::as_array).into_iter().flatten().map(|entry|entry.get("gslb_params").map(|value|diagnostic_config_shape(value,0))).collect::<Vec<_>>());
        match plan.decode(&body) {
            Ok(resolved)=>{
                report["decoded"]=json!(true);
                report["responses"]=json!(resolved.iter().map(|media|json!({"status_code":media.status_code,"media_code_present":!media.media_code.is_empty(),
                    "metadata_fields":media.metadata.as_ref().and_then(Value::as_object).map(|object|object.keys().cloned().collect::<Vec<_>>()),
                    "servers":media.servers.iter().map(server_shape).collect::<Vec<_>>(),"cast_servers":media.cast_servers.iter().map(server_shape).collect::<Vec<_>>() })).collect::<Vec<_>>());
            },
            Err(error)=>{report["decoded"]=json!(false);report["error"]=json!(error);},
        }
        save_json(&analysis.join("vod-slb-diagnostic.safe.json"),&report).unwrap();
        println!("VOD SLB diagnostic complete; redacted structure saved, no media fetched.");
    }
    #[test]
    #[ignore]
    fn diagnose_authorized_slb_config_shape(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("slb_config"),"Explicit SLB configuration diagnostic flag required");
        assert!(data_directory().unwrap().join("session.json").exists(),"Existing own session required");
        let mut api=Api::new().unwrap();
        assert!(!api.session.user_id.is_empty() && !api.session.user_token.is_empty(),"Existing authorized session required");
        api.initialized=true;
        let slb=api.authenticated_post("y",json!({"encMediaSupported":1,"liveCodeList":[format!("{}_live",api.session.portal_code)],
            "type":"merge","appVer":api.protocol.api_version().to_string(),"lang":"pt","reserve1":"","pipFlag":"0",
            "hasPay":api.session.has_pay,"userIdentity":api.session.user_identity})).unwrap();
        let entry_shapes:Vec<Value>=slb.get("cdn_list").and_then(Value::as_array).into_iter().flatten().map(|entry|json!({
            "cdn_type":entry.get("cdn_type"),"business_is_live":entry.get("tag").and_then(Value::as_str)==Some("live"),
            "gslb_params":entry.get("gslb_params").map(|value|diagnostic_config_shape(value,0))
        })).collect();
        let report=json!({"requests":1,"operation":"getSlbInfo","activation_requests":0,"media_fetches":0,
            "top_fields":slb.as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>()),"play_params":slb.get("play_params").map(|value|diagnostic_config_shape(value,0)),"entries":entry_shapes});
        save_json(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/slb-config-shape.safe.json"),&report).unwrap();
        println!("SLB configuration keys/types saved; exactly one bootstrap read, no media fetched.");
    }
    #[test]
    fn explicit_source_is_selected_without_calling_slb(){
        let data=json!({"episodeList":[{"programContentId":"synthetic-title","totalMovieList":[
            {"play_url":"https://example.invalid/movie.m3u8","licenseList":[{"license":"opaque"}]}
        ]}]});
        let source=prefer_explicit_playback(&data,"synthetic-title",None,||panic!("Explicit media must not request SLB")).unwrap();
        assert_eq!(source.url.as_str(),"https://example.invalid/movie.m3u8");
        assert!(source.headers.is_empty());
        let fallback_calls=std::cell::Cell::new(0);
        let error=prefer_explicit_playback(&json!({"licenseList":[{"license":"opaque"}]}),"synthetic-title",None,||{
            fallback_calls.set(fallback_calls.get()+1);
            Err("synthetic SLB error".into())
        });
        assert!(matches!(error,Err(message) if message=="synthetic SLB error"));
        assert_eq!(fallback_calls.get(),1);
    }

    #[test]
    fn explicit_source_fields_exclude_other_episodes_posters_and_licenses(){
        let data=json!({
            "posterList":[{"play_url":"https://example.invalid/poster.jpg"}],
            "licenseList":[{"mediaUrl":"https://example.invalid/not-media"}],
            "cdn_list":[{"playUrl":"https://example.invalid/bootstrap"}],
            "subtitleList":[{"play_url":"https://example.invalid/subtitles.vtt"}],
            "episodeList":[
                {"programContentId":"other","episodeNumber":1,"play_url":"https://example.invalid/other.mp4"},
                {"programContentId":"selected","episodeNumber":2,"totalMovieList":[
                    {"url":"https://example.invalid/generic","movieList":[{"playUrl":"https://example.invalid/selected.mp4"}]}
                ]}
            ]
        });
        let urls=explicit_media_urls(&data,"selected",Some(2));
        assert_eq!(urls.len(),1);
        assert_eq!(urls[0].as_str(),"https://example.invalid/selected.mp4");
        assert!(explicit_media_urls(&data,"absent",Some(3)).is_empty());
        let invalid=json!({"play_url":"file:///private","playUrl":"javascript:alert(1)","mediaUrl":"https://user:password@example.invalid/video", "url":"https://example.invalid/generic","license":"https://example.invalid/opaque"});
        assert!(explicit_media_urls(&invalid,"synthetic-title",None).is_empty());
        assert_eq!(explicit_media_urls(&json!({"data":{"mediaUrl":"https://example.invalid/direct.mp4"}}),"synthetic-title",None).len(),1);
    }

    #[test]
    fn explicit_source_traversal_is_bounded_and_deduplicated(){
        let duplicate=json!({"play_url":"https://example.invalid/direct.mp4","playUrl":"https://example.invalid/direct.mp4"});
        assert_eq!(explicit_media_urls(&duplicate,"synthetic-title",None).len(),1);
        let many=Value::Array((0..100).map(|n|json!({"mediaUrl":format!("https://example.invalid/{n}.mp4")})).collect());
        assert_eq!(explicit_media_urls(&many,"synthetic-title",None).len(),64);
        let mut too_deep=json!({"play_url":"https://example.invalid/deep.mp4"});
        for _ in 0..20{too_deep=json!({"data":too_deep});}
        assert!(explicit_media_urls(&too_deep,"synthetic-title",None).is_empty());
    }

    #[test]
    fn resolved_video_must_match_requested_media_and_success_status(){
        let requested=vec![crate::ranger::VodAuthorization{auth:"synthetic-auth".into(),license:"synthetic-license".into(),media_code:"requested".into()}];
        let server=||crate::ranger::SlbServer{tag:String::new(),server_code:String::new(),media_url:"https://example.invalid/movie.mp4".into(),idx_url:String::new(),snapshot_url:String::new(),torrent_url:String::new()};
        let mut resolved=vec![crate::ranger::ResolvedMedia{media_code:"unrelated".into(),status_code:200,metadata:None,servers:vec![server()],cast_servers:vec![]}];
        assert!(select_vod_server(&resolved,&requested,"requested").is_err());
        resolved[0].media_code="requested".into();
        resolved[0].status_code=403;
        assert!(select_vod_server(&resolved,&requested,"requested").is_err());
        resolved[0].status_code=200;
        assert!(select_vod_server(&resolved,&requested,"requested").is_ok());
        resolved[0].servers.clear();
        // Cast and bootstrap URLs are not interchangeable with a media source.
        resolved[0].cast_servers.push(server());
        assert!(select_vod_server(&resolved,&requested,"requested").is_err());
    }

    #[test]
    fn selected_quality_is_not_replaced_by_another_successful_variant(){
        let auth=|id:&str|crate::ranger::VodAuthorization{auth:"synthetic-auth".into(),license:"synthetic-license".into(),media_code:id.into()};
        let server=||crate::ranger::SlbServer{tag:String::new(),server_code:String::new(),media_url:"https://example.invalid/movie.mp4".into(),idx_url:String::new(),snapshot_url:String::new(),torrent_url:String::new()};
        let response=|id:&str,status_code|crate::ranger::ResolvedMedia{media_code:id.into(),status_code,metadata:None,servers:vec![server()],cast_servers:vec![]};
        let authorizations=vec![auth("other-quality"),auth("selected-quality")];
        let mut resolved=vec![response("other-quality",200),response("selected-quality",403)];
        assert!(select_vod_server(&resolved,&authorizations,"selected-quality").is_err());
        resolved[1].status_code=200;
        resolved[1].servers[0].media_url="https://example.invalid/selected.mp4".into();
        assert!(select_vod_server(&resolved,&authorizations,"selected-quality").unwrap().media_url.ends_with("/selected.mp4"));
        assert!(select_vod_server(&resolved,&authorizations,"unrequested").is_err());
    }

    #[test]
    fn parameters_are_bounded(){
        assert!(page(&json!({"page":0})).is_err());
        assert!(page(&json!({"page":10001})).is_err());
        assert!(kind(&json!({"kind":"anything"})).is_err());
        assert_eq!(page(&json!({})).unwrap(),1);
        assert!(catalog_has_more("live",&json!({"channelListTotalSize":50}),2,50));
        assert!(!catalog_has_more("live",&json!({"channelListTotalSize":12}),3,12));
        assert!(!catalog_has_more("vod",&json!({"assetListTotalSize":50}),1,50));
    }
    #[test]
    fn persistence_retains_installation_and_favorites(){
        let directory=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis").join(format!("xuper-offline-test-{}",uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path=directory.join("state.json");
        save_json(&path,&json!({"installation":"synthetic-1"})).unwrap();
        save_json(&path,&json!({"installation":"synthetic-1","favorite":"example"})).unwrap();
        let stored:Value=read_json(&path).unwrap();
        assert_eq!(stored["installation"],"synthetic-1");
        assert_eq!(stored["favorite"],"example");
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
