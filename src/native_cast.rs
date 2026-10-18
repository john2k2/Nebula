//! In-process Chromecast discovery and playback through the bundled LibVLC 3 API.
//! UI receives opaque renderer IDs; media URLs and session capabilities stay in Rust.
use std::{ffi::{c_char,c_int,c_void,CString},sync::{Mutex,OnceLock,atomic::{AtomicBool,AtomicU64,Ordering}},time::Instant};
use serde_json::{Value,json};
use crate::native_player::{Library,runtime,text};
type Ptr=*mut c_void;
const ADDED:c_int=0x502;
const REMOVED:c_int=0x503;
// Prefix of libvlc_event_t. The first member of either renderer-event union is
// the item pointer. repr(C) preserves the union's native pointer alignment.
#[repr(C)] pub(crate) struct RendererEvent { kind:c_int,object:Ptr,item:Ptr }
struct Device { id:String,name:String,item:Ptr }
unsafe impl Send for Device {}
struct Registry {
    devices:Mutex<Vec<Device>>,
    hold:unsafe extern "C" fn(Ptr)->Ptr,release:unsafe extern "C" fn(Ptr),
    name:unsafe extern "C" fn(Ptr)->*const c_char,kind:unsafe extern "C" fn(Ptr)->*const c_char,flags:unsafe extern "C" fn(Ptr)->c_int,
}
unsafe extern "C" fn discovered(event:*const RendererEvent,opaque:Ptr) {
    // Never unwind across the C callback or acquire the CastEngine mutex here.
    let _=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||unsafe{
        let(Some(event),Some(registry))=(event.as_ref(),(opaque as *const Registry).as_ref())else{return;};
        if event.item.is_null(){return;}let Ok(mut devices)=registry.devices.lock()else{return;};
        if event.kind==ADDED {
            if devices.len()>=64||devices.iter().any(|d|d.item==event.item)||(registry.flags)(event.item)&2==0||text((registry.kind)(event.item))!="chromecast"{return;}
            let name=text((registry.name)(event.item));
            let item=(registry.hold)(event.item);if item.is_null(){return;}
            devices.push(Device{id:uuid::Uuid::new_v4().simple().to_string(),name,item});
        }else if event.kind==REMOVED {
            if let Some(index)=devices.iter().position(|d|d.item==event.item){let old=devices.remove(index);(registry.release)(old.item);}
        }
    }));
}
struct CastEngine {
    lib:Library,instance:Ptr,discoverer:Ptr,events:Ptr,attached:Vec<c_int>,registry:Box<Registry>,
    player:Ptr,media:Ptr,session:Option<String>,device:String,title:String,generation:u64,
    desired_audio:Option<u64>,audio_applied:bool,started:Instant,context:Value,device_id:String,requested_language:String,
}
unsafe impl Send for CastEngine {}
static ENGINE:OnceLock<Mutex<Option<CastEngine>>>=OnceLock::new();
static BUSY:AtomicBool=AtomicBool::new(false);
static GENERATION:AtomicU64=AtomicU64::new(0);
fn engine()->&'static Mutex<Option<CastEngine>> {ENGINE.get_or_init(||Mutex::new(None))}
impl CastEngine {
    fn new()->Result<Self,String>{
        let lib=unsafe{Library::load(&runtime().ok_or("Falta el motor VLC del paquete de Nébula.")?)?};
        let port=std::net::TcpListener::bind("0.0.0.0:0").and_then(|listener|listener.local_addr()).map_err(|_|"No se pudo preparar el puerto de Chromecast.")?.port();
        let args=["--ignore-config".into(),"--quiet".into(),"--no-video-title-show".into(),"--no-media-library".into(),"--no-metadata-network-access".into(),"--no-sub-autodetect-file".into(),"--sout-chromecast-show-perf-warning=0".into(),format!("--sout-chromecast-http-port={port}")];
        let args:Vec<_>=args.iter().map(|s:&String|CString::new(s.as_str()).unwrap()).collect();let ptrs:Vec<_>=args.iter().map(|s|s.as_ptr()).collect();
        let instance=unsafe{(lib.new)(ptrs.len() as c_int,ptrs.as_ptr())};if instance.is_null(){return Err("No se pudo iniciar Chromecast en VLC.".into());}
        let registry=Box::new(Registry{devices:Mutex::new(vec![]),hold:lib.renderer_hold,release:lib.renderer_item_release,name:lib.renderer_name,kind:lib.renderer_type,flags:lib.renderer_flags});
        let mut this=Self{lib,instance,registry,discoverer:std::ptr::null_mut(),events:std::ptr::null_mut(),attached:vec![],player:std::ptr::null_mut(),media:std::ptr::null_mut(),session:None,device:String::new(),title:String::new(),generation:0,desired_audio:None,audio_applied:false,started:Instant::now(),context:Value::Null,device_id:String::new(),requested_language:String::new()};
        unsafe{
            this.discoverer=(this.lib.renderer_new)(instance,c"microdns_renderer".as_ptr());
            if this.discoverer.is_null(){return Err("El paquete VLC no incluye detección de Chromecast.".into());}
            this.events=(this.lib.renderer_events)(this.discoverer);
            for event in [ADDED,REMOVED]{
                if(this.lib.event_attach)(this.events,event,discovered,(&*this.registry as *const Registry).cast_mut().cast())!=0{return Err("No se pudo escuchar la detección de televisores.".into());}
                this.attached.push(event);
            }
            if(this.lib.renderer_start)(this.discoverer)!=0{return Err("No se pudo buscar Chromecast en la red. Comprueba la conexión de esta PC.".into());}
        }Ok(this)
    }
    fn devices(&self)->Result<Value,String>{
        let devices=self.registry.devices.lock().map_err(|_|"Se interrumpió la detección de televisores.")?;
        let mut rows:Vec<_>=devices.iter().map(|d|json!({"id":d.id,"name":d.name,"type":"chromecast"})).collect();rows.sort_by_key(|v|v["name"].as_str().unwrap_or("").to_lowercase());
        Ok(json!({"devices":rows,"native":true,"scanning":true}))
    }
    fn has(&self,id:&str)->bool {self.registry.devices.lock().is_ok_and(|devices|devices.iter().any(|d|d.id==id))}
    fn stop(&mut self){
        unsafe{if !self.player.is_null(){(self.lib.stop)(self.player);(self.lib.player_release)(self.player);self.player=std::ptr::null_mut();}
            if !self.media.is_null(){(self.lib.media_release)(self.media);self.media=std::ptr::null_mut();}}
        if let Some(session)=self.session.take(){let _=crate::api::handle("playback.stop",&json!({"session_id":session}));}
        self.desired_audio=None;self.audio_applied=false;
    }
    fn open(&mut self,device:&str,ticket:crate::media_proxy::PlaybackTicket,params:&Value,selection:&Value,generation:u64)->Result<Value,String>{
        self.stop();self.session=Some(ticket.session_id);self.generation=generation;
        let (item,name)={let devices=self.registry.devices.lock().map_err(|_|"Se interrumpió la detección.")?;let d=devices.iter().find(|d|d.id==device).ok_or("El televisor dejó de estar disponible. Vuelve a buscarlo.")?;unsafe{((self.lib.renderer_hold)(d.item),d.name.clone())}};
        let result=(||unsafe{
            let address=CString::new(ticket.url).map_err(|_|"La fuente de reproducción no es válida.")?;
            self.media=(self.lib.media_new)(self.instance,address.as_ptr());if self.media.is_null(){return Err("No se pudo preparar el video para el televisor.".into());}
            let position=valid_position(&params["start_time"]).unwrap_or(0.0);
            let mut options=vec![":network-caching=1500".into(),":http-reconnect".into(),":no-sub-autodetect-file".into(),format!(":start-time={position}"),":sout-all=0".into()];
            if let Some(index)=selection["audio_track_index"].as_u64().filter(|n|*n<128){options.push(format!(":audio-track={index}"));self.desired_audio=Some(index);}
            if let Some(language)=selection["audio_language"].as_str().filter(|s|s.len()<32&&s.bytes().all(|v|v.is_ascii_alphabetic()||v==b'-')){options.push(format!(":audio-language={language}"));}
            for option in options{(self.lib.media_option)(self.media,CString::new(option).unwrap().as_ptr());}
            self.player=(self.lib.player_new)(self.instance);if self.player.is_null(){return Err("No se pudo abrir el reproductor del televisor.".into());}
            if(self.lib.set_renderer)(self.player,item)!=0{return Err("VLC no pudo seleccionar ese Chromecast.".into());}
            (self.lib.set_media)(self.player,self.media);
            self.device=name;self.device_id=device.into();self.title=params["title"].as_str().unwrap_or("Nébula").chars().take(300).collect();self.started=Instant::now();
            self.context=json!({"id":params["id"],"kind":params["kind"],"title":self.title,"quality":selection["quality"],"audio_language":selection["audio_language"]});self.requested_language=selection["audio_language"].as_str().unwrap_or("").into();
            if(self.lib.play)(self.player)!=0{return Err("No se pudo iniciar el envío al televisor.".into());}
            Ok(self.status())
        })();unsafe{(self.lib.renderer_item_release)(item);}if result.is_err(){self.stop();}result
    }
    fn audio(&self)->Vec<Value>{if self.player.is_null(){vec![]}else{crate::native_player::cast_audio_tracks(&self.lib,self.media,self.player)}}
    fn status(&mut self)->Value{
        let active=!self.player.is_null();let tracks=self.audio();
        if active&&!self.audio_applied{if let Some(id)=choose_audio(&tracks,&self.requested_language,self.desired_audio){
            self.audio_applied=unsafe{(self.lib.audio_set)(self.player,id as c_int)}==0;
        }}
        let (state,time,duration,seekable,audio)=if active{unsafe{((self.lib.state)(self.player),(self.lib.time)(self.player).max(0) as f64/1000.0,(self.lib.length)(self.player).max(0) as f64/1000.0,(self.lib.seekable)(self.player)!=0,(self.lib.audio_get)(self.player))}}else{(0,0.0,0.0,false,-1)};
        json!({"active":active,"preparing":BUSY.load(Ordering::Acquire),"generation":GENERATION.load(Ordering::Acquire),"device":self.device,"device_id":self.device_id,"item":self.context,"title":self.title,"state":state,"time":time,"duration":duration,"seekable":seekable,"audio_tracks":tracks,"audio_track":audio,"audio_applied":self.audio_applied,"native":true,"waiting_seconds":self.started.elapsed().as_secs()})
    }
}
impl Drop for CastEngine{fn drop(&mut self){self.stop();unsafe{
    if !self.discoverer.is_null(){(self.lib.renderer_stop)(self.discoverer);for event in &self.attached{(self.lib.event_detach)(self.events,*event,discovered,(&*self.registry as *const Registry).cast_mut().cast());}(self.lib.renderer_release)(self.discoverer);}
    if let Ok(mut devices)=self.registry.devices.lock(){for device in devices.drain(..){(self.lib.renderer_item_release)(device.item);}}
    (self.lib.instance_release)(self.instance);
}}}
fn valid_position(value:&Value)->Option<f64>{value.as_f64().filter(|v|v.is_finite()&&*v>=0.0&&*v<=7.0*86400.0)}
fn language(value:&str)->&str{let short=value.split(['-','_']).next().unwrap_or("");match short{"spa"=>"es","eng"=>"en","kor"=>"ko","por"=>"pt","fra"|"fre"=>"fr","jpn"=>"ja","deu"|"ger"=>"de",other=>other}}
fn choose_audio(tracks:&[Value],wanted:&str,index:Option<u64>)->Option<i64>{
    let wanted=language(wanted);let by_language=tracks.iter().find(|row|!wanted.is_empty()&&language(row["language"].as_str().unwrap_or(""))==wanted);
    by_language.or_else(||if wanted.is_empty()||tracks.iter().all(|row|row["language"].as_str().unwrap_or("").is_empty()){index.and_then(|n|tracks.get(n as usize))}else{None}).and_then(|row|row["id"].as_i64())
}
fn selected_device(params:&Value)->Result<&str,String>{params["device_id"].as_str().filter(|s|s.len()==32&&s.bytes().all(|b|b.is_ascii_hexdigit())).ok_or("Elige un televisor de la lista.".into())}
fn prepare_source(params:&Value)->Result<(crate::media_proxy::PlaybackTicket,Value),String>{
    let kind=params["kind"].as_str().filter(|v|*v=="vod").ok_or("Elige una película o un episodio para enviar.")?;
    let id=params["id"].as_str().filter(|v|!v.is_empty()&&v.len()<=256).ok_or("El título seleccionado no es válido.")?;
    let request=crate::api::cast_playback_params(kind,id,params["quality"].as_str(),params["audio_language"].as_str())?;
    let value=crate::api::handle("playback",&request)?;
    let session=value["session_id"].as_str().ok_or("No se pudo preparar la sesión de reproducción.")?;
    let result=crate::api::native_cast_ticket(session).map(|ticket|(ticket,value["selection"].clone()));
    if result.is_err(){let _=crate::api::handle("playback.stop",&json!({"session_id":session}));}result
}
struct Preparing;impl Drop for Preparing{fn drop(&mut self){BUSY.store(false,Ordering::Release);}}
pub fn command(command:&str,params:&Value)->Result<Value,String>{
    if command=="cast.open"{return Err("Elige el televisor desde el selector de Chromecast de Nébula.".into());}
    if command=="cast.start"{
        let device=selected_device(params)?;
        if BUSY.swap(true,Ordering::AcqRel){return Err("Ya se está preparando un envío. Puedes cancelarlo desde el control.".into());}let _busy=Preparing;
        let generation={let mut guard=engine().lock().map_err(|_|"Se interrumpió Chromecast.")?;if guard.is_none(){*guard=Some(CastEngine::new()?);}let e=guard.as_mut().unwrap();if !e.has(device){return Err("Ese televisor ya no está disponible. Vuelve a buscarlo.".into());}e.stop();GENERATION.fetch_add(1,Ordering::AcqRel)+1};
        let (ticket,selection)=prepare_source(params)?;
        let mut guard=engine().lock().map_err(|_|"Se interrumpió Chromecast.")?;
        if GENERATION.load(Ordering::Acquire)!=generation{let _=crate::api::handle("playback.stop",&json!({"session_id":ticket.session_id}));return Err("Se canceló el envío.".into());}
        let e=guard.as_mut().ok_or("Se cerró Chromecast.")?;let result=e.open(device,ticket,params,&selection,generation);if result.is_err(){e.stop();}return result;
    }
    let mut guard=engine().lock().map_err(|_|"Se interrumpió Chromecast.")?;
    if command=="cast.devices"&&guard.is_none(){*guard=Some(CastEngine::new()?);}
    let Some(e)=guard.as_mut()else{return Ok(json!({"active":false,"preparing":false,"generation":GENERATION.load(Ordering::Acquire)}));};
    if command=="cast.devices"{return e.devices();}
    if command=="cast.status"{return Ok(e.status());}
    if params["generation"].as_u64()!=Some(GENERATION.load(Ordering::Acquire)){return Err("Este control pertenece a un envío anterior. Actualiza su estado.".into());}
    if command=="cast.stop"{GENERATION.fetch_add(1,Ordering::AcqRel);e.stop();return Ok(e.status());}
    if e.player.is_null(){return Err("No hay un video conectado al televisor.".into());}
    unsafe{match command{
        "cast.pause"=>(e.lib.pause)(e.player,i32::from(params["paused"].as_bool().unwrap_or(true))),
        "cast.seek"=>{let time=valid_position(&params["time"]).ok_or("La posición no es válida.")?;if(e.lib.seekable)(e.player)==0{return Err("El televisor todavía no permite cambiar la posición.".into());}(e.lib.set_time)(e.player,(time*1000.0).round() as i64);},
        "cast.volume"=>{let volume=params["volume"].as_f64().filter(|v|v.is_finite()&&*v>=0.0&&*v<=1.0).ok_or("El volumen no es válido.")?;if(e.lib.volume)(e.player,(volume*100.0).round() as c_int)!=0{return Err("El receptor no admite cambiar el volumen desde VLC.".into());}},
        "cast.audio"=>{let track=params["track"].as_i64().filter(|id|e.audio().iter().any(|t|t["id"].as_i64()==Some(*id))).ok_or("La pista de audio no está disponible.")?;if(e.lib.audio_set)(e.player,track as c_int)!=0{return Err("No se pudo aplicar esa pista de audio al envío.".into());}e.audio_applied=true;},
        _=>return Err("Control de Chromecast no admitido.".into()),
    }}Ok(e.status())
}
pub fn shutdown(){GENERATION.fetch_add(1,Ordering::AcqRel);if let Ok(mut guard)=engine().lock(){*guard=None;}}

#[cfg(test)] mod tests{
    use super::*;
    #[test]fn controls_reject_unbounded_positions_and_unknown_devices(){assert_eq!(valid_position(&json!(125.5)),Some(125.5));for v in [json!(-1),json!(700000),json!("12"),Value::Null]{assert!(valid_position(&v).is_none());}for id in ["", "192.168.1.12","http://127.0.0.1/","not-a-renderer"]{assert!(selected_device(&json!({"device_id":id})).is_err());}assert!(selected_device(&json!({"device_id":"ab".repeat(16)})).is_ok());}
    #[test]fn event_prefix_matches_libvlc_64_bit_abi(){assert_eq!(std::mem::offset_of!(RendererEvent,item),16);}
    #[test]fn audio_language_wins_over_a_different_bundle_index(){let tracks=vec![json!({"id":11,"language":"eng"}),json!({"id":22,"language":"spa"})];assert_eq!(choose_audio(&tracks,"es",Some(0)),Some(22));assert_eq!(choose_audio(&tracks,"ko",Some(0)),None);assert_eq!(choose_audio(&[json!({"id":4})],"es",Some(0)),Some(4));}
    #[test]#[ignore="Starts real mDNS discovery on the user's local network"]fn discover_real_chromecast(){
        command("cast.devices",&json!({})).unwrap();let began=Instant::now();let mut result=json!({});
        while began.elapsed().as_secs()<20{result=command("cast.devices",&json!({})).unwrap();if result["devices"].as_array().is_some_and(|d|!d.is_empty()){break;}std::thread::sleep(std::time::Duration::from_millis(500));}
        let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("analysis/vlc-integration/native-cast-discovery.local.json");std::fs::write(path,serde_json::to_vec_pretty(&result).unwrap()).unwrap();println!("Native discovery: {} video receivers",result["devices"].as_array().map_or(0,Vec::len));shutdown();
    }
}
