//! Ranger starts every eligible VOD Entry. An HTTP rejection belongs to that
//! Entry, whereas a transport failure may use its spare. These are different
//! transitions; see the original ARM64 scheduler/callback synthetic oracle.
use crate::ranger::{MediaRequest, SlbRequest};
use std::{collections::BTreeMap, io::Read, sync::{Arc, mpsc, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

static ACTIVE_WORKERS:AtomicUsize=AtomicUsize::new(0);
struct WorkerPermit;
impl WorkerPermit {
    fn acquire()->Option<Self>{
        let mut active=ACTIVE_WORKERS.load(Ordering::Acquire);
        loop {
            if active>=32{return None;}
            match ACTIVE_WORKERS.compare_exchange_weak(active,active+1,Ordering::AcqRel,Ordering::Acquire){Ok(_)=>return Some(Self),Err(current)=>active=current}
        }
    }
}
impl Drop for WorkerPermit{fn drop(&mut self){ACTIVE_WORKERS.fetch_sub(1,Ordering::AcqRel);}}
struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop{fn drop(&mut self){self.0.store(true,Ordering::Release);}}

struct Failure { message: String, transport: bool }
impl Failure {
    fn service(message: impl Into<String>) -> Self { Self { message: message.into(), transport:false } }
    fn transport(message: impl Into<String>) -> Self { Self { message: message.into(), transport:true } }
}

pub fn resolve(plans: Vec<SlbRequest>, selected_media: &str, deadline: Instant) -> Result<MediaRequest,String> {
    resolve_with(plans, selected_media, deadline, request)
}

fn resolve_with<F>(plans:Vec<SlbRequest>, selected_media:&str, deadline:Instant, fetch:F)->Result<MediaRequest,String>
where F:Fn(&SlbRequest,&str,Instant)->Result<MediaRequest,Failure> + Send + Sync + 'static {
    let mut entries:BTreeMap<usize,Vec<SlbRequest>>=BTreeMap::new();
    for plan in plans { entries.entry(plan.entry_index).or_default().push(plan); }
    if entries.is_empty() { return Err("El servidor no ofreció una ruta de reproducción compatible.".into()); }
    // Bound independent work even if a remote configuration is malformed.
    if entries.len()>8 { return Err("La configuración contiene demasiadas rutas de reproducción.".into()); }
    crate::protocol::remaining_timeout(Some(deadline),Duration::from_secs(45))?;
    {
        let fetch=Arc::new(fetch);
        let selected_media=Arc::new(selected_media.to_string());
        let cancelled=Arc::new(AtomicBool::new(false));
        let _cancel_on_return=StopOnDrop(cancelled.clone());
        let (sender,receiver)=mpsc::channel();
        for (_,mut entry) in entries {
            entry.sort_by_key(|plan|plan.endpoint_is_spare);
            let sender=sender.clone();
            let fetch=fetch.clone();
            let selected_media=selected_media.clone();
            let cancelled=cancelled.clone();
            let permit=WorkerPermit::acquire().ok_or("El reproductor sigue terminando otra solicitud. Inténtalo en unos segundos.")?;
            std::thread::Builder::new().name("xuper-vod-route".into()).spawn(move|| {
                let _permit=permit;
                let mut result=Err(Failure::service("No hay una ruta principal de vídeo disponible."));
                for (index,plan) in entry.iter().enumerate() {
                    if index==0 && plan.endpoint_is_spare { break; }
                    if index>0 && !result.as_ref().err().is_some_and(|failure|failure.transport) { break; }
                    if cancelled.load(Ordering::Acquire) || Instant::now()>=deadline { break; }
                    result=fetch(plan,&selected_media,deadline);
                    if result.is_ok() { break; }
                }
                let _=sender.send(result.map_err(|failure|failure.message));
            }).map_err(|_|"No se pudo iniciar la preparación del vídeo.".to_string())?;
        }
        drop(sender);
        let mut last_error="No se encontró una fuente de vídeo disponible.".to_string();
        loop {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(source))=>return Ok(source),
                Ok(Err(error))=>last_error=error,
                Err(mpsc::RecvTimeoutError::Disconnected)=>return Err(last_error),
                Err(mpsc::RecvTimeoutError::Timeout)=>return Err("La preparación del vídeo tardó demasiado. Inténtalo de nuevo.".into()),
            }
        }
    }
}

fn request(plan:&SlbRequest,selected_media:&str,deadline:Instant)->Result<MediaRequest,Failure> {
    #[cfg(debug_assertions)] crate::playback_diagnostics::verify_vod_plan(plan);
    crate::playback_diagnostics::record_plan(&plan.url);
    let (builder,_)=crate::protocol::media_client_builder_until(&plan.url,Some(deadline)).map_err(Failure::transport)?;
    let client=builder.build().map_err(|_|Failure::transport("No se pudo preparar la conexión segura de vídeo."))?;
    let timeout=crate::protocol::remaining_timeout(Some(deadline),Duration::from_secs(20)).map_err(Failure::service)?;
    let mut request=client.get(plan.url.clone()).timeout(timeout);
    for (name,value) in &plan.headers { request=request.header(name,value); }
    let response=request.send().map_err(|error| {
        let message=if error.is_timeout(){"El servidor tardó demasiado en preparar el vídeo."}else{"No se pudo conectar de forma segura con el servidor de vídeo."};
        if Instant::now()>=deadline { Failure::service(message) } else { Failure::transport(message) }
    })?;
    crate::playback_diagnostics::record_response(&response);
    if response.status().as_u16()!=200 {
        return Err(Failure::service("El servicio de video rechazó la solicitud. Vuelve a intentarlo más tarde."));
    }
    let mut bytes=Vec::new();
    response.take(65_537).read_to_end(&mut bytes).map_err(|_|{
        let message="La respuesta del servidor de vídeo quedó incompleta.";
        if Instant::now()>=deadline{Failure::service(message)}else{Failure::transport(message)}
    })?;
    let wire=String::from_utf8(bytes).map_err(|_|Failure::service("El servidor devolvió una respuesta de reproducción no reconocida."))?;
    let resolved=plan.decode(&wire).map_err(Failure::service)?;
    let server=crate::api::select_vod_server(&resolved,&plan.authorizations,selected_media).map_err(Failure::service)?;
    let now=SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_|Failure::service("El reloj de Windows no tiene una fecha válida."))?;
    let now_ms=i64::try_from(now.as_millis()).map_err(|_|Failure::service("La fecha de Windows está fuera del intervalo compatible."))?;
    let mut media=crate::ranger::prepare_vod_media(server,0,now_ms).map_err(Failure::service)?;
    // Native SetEnv uses the same player identity for SLB and media. The relay
    // keeps this header private and limits it to the authorized source origin.
    media.headers.extend(plan.headers.iter().filter(|(name,_)|name.eq_ignore_ascii_case("Ranger-Id")).cloned());
    verify_random_access(&media, deadline)?;
    Ok(media)
}

// An SLB reply can point at a CDN that ignores Range and sends a whole MP4.
// VLC then has to read through the file to find a trailing moov box. Keep the
// independent authorized routes racing until one actually supports seeking.
fn verify_random_access(media:&MediaRequest,deadline:Instant)->Result<(),Failure>{
    let mut url=media.url.clone();
    let credential_origin=url.origin().ascii_serialization();
    for _ in 0..6 {
        let (builder,_)=crate::protocol::media_client_builder_until(&url,Some(deadline)).map_err(Failure::transport)?;
        let client=builder.redirect(reqwest::redirect::Policy::none()).build()
            .map_err(|_|Failure::transport("No se pudo comprobar la ruta de vídeo."))?;
        let timeout=crate::protocol::remaining_timeout(Some(deadline),Duration::from_secs(5)).map_err(Failure::service)?;
        let mut request=client.get(url.clone()).timeout(timeout).header("Accept-Encoding","identity").header("Range","bytes=0-0");
        if url.origin().ascii_serialization()==credential_origin {
            for (name,value) in &media.headers { request=request.header(name,value); }
        }
        let response=request.send().map_err(|_|Failure::transport("La ruta de vídeo no respondió a la comprobación de avance."))?;
        if matches!(response.status().as_u16(),301|302|303|307|308){
            let location=response.headers().get("Location").and_then(|v|v.to_str().ok())
                .ok_or_else(||Failure::service("La ruta de vídeo devolvió una redirección inválida."))?;
            url=url.join(location).map_err(|_|Failure::service("La ruta de vídeo devolvió una redirección inválida."))?;
            continue;
        }
        let seekable=valid_range_probe(response.status().as_u16(),response.headers().get("Content-Range").and_then(|v|v.to_str().ok()));
        crate::native_player::diagnostic("vod-route-range-probe",serde_json::json!({"status":response.status().as_u16(),"seekable":seekable}));
        return if seekable {Ok(())} else {Err(Failure::service("Esta ruta de vídeo no permite cargar ni buscar posiciones correctamente. Vuelve a intentarlo."))};
    }
    Err(Failure::service("La ruta de vídeo redirigió demasiadas veces."))
}

fn valid_range_probe(status:u16,range:Option<&str>)->bool{
    if status!=206{return false;}
    let Some(range)=range.and_then(|range|range.strip_prefix("bytes 0-")) else{return false;};
    let Some((end,total))=range.split_once('/') else{return false;};
    match (end.parse::<u64>(),total.parse::<u64>()) {(Ok(end),Ok(total))=>end<total&&total>0,_=>false}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex,Barrier};

    #[test]
    fn an_ignored_range_is_not_an_usable_seekable_route(){
        assert!(!valid_range_probe(200,None));
        assert!(!valid_range_probe(200,Some("bytes 0-0/600000000")));
        assert!(!valid_range_probe(206,Some("bytes 10-20/100")));
        assert!(!valid_range_probe(206,Some("bytes 0-100/100")));
        assert!(!valid_range_probe(206,Some("bytes 0-0/*")));
        assert!(valid_range_probe(206,Some("bytes 0-0/600000000")));
        assert!(valid_range_probe(206,Some("bytes 0-4095/600000000")));
    }

    fn plans()->Vec<SlbRequest>{
        let entry=|name:&str,number|json!({"cdn_type":"1","tag":"vod","cdn_id_mark":name,"serial_number":number,
            "main_addr":format!("https://{name}.example.invalid"),"spared_addr":format!("https://spare-{name}.example.invalid"),
            "url_list":[{"url":"app_id=synthetic&tag=test&auth=synthetic"}]} );
        crate::ranger::prepare_vod_requests(&json!({"cdn_list":[entry("one",0),entry("two",1)]}),
            &[crate::ranger::RangerMedia{media_code:"selected".into(),license:"app_id=synthetic&tag=test&license=synthetic".into()}]).unwrap()
    }
    fn source()->MediaRequest { MediaRequest{url:reqwest::Url::parse("https://media.example.invalid/movie.mp4").unwrap(),headers:vec![]} }

    #[test]
    fn rejected_first_entry_does_not_hide_another_working_entry_or_use_spares(){
        let calls=Arc::new(Mutex::new(vec![]));
        let observed=calls.clone();
        let both=Arc::new(Barrier::new(2));
        let result=resolve_with(plans(),"selected",Instant::now()+Duration::from_secs(3),move|plan,selected,_|{
            assert_eq!(selected,"selected");
            observed.lock().unwrap().push((plan.entry_name.clone(),plan.endpoint_is_spare));
            both.wait();
            if plan.entry_name=="one" { Err(Failure::service("HTTP400")) } else { Ok(source()) }
        });
        assert!(result.is_ok());
        let mut calls=calls.lock().unwrap().clone(); calls.sort();
        assert_eq!(calls,vec![("one".into(),false),("two".into(),false)]);
    }

    #[test]
    fn spare_is_only_used_for_its_own_transport_failure(){
        let calls=Arc::new(Mutex::new(vec![]));
        let observed=calls.clone();
        let both=Arc::new(Barrier::new(2));
        let result=resolve_with(plans(),"selected",Instant::now()+Duration::from_secs(3),move|plan,_,_|{
            observed.lock().unwrap().push((plan.entry_name.clone(),plan.endpoint_is_spare));
            if !plan.endpoint_is_spare {both.wait();}
            if plan.entry_name=="one" && !plan.endpoint_is_spare { Err(Failure::transport("connect")) }
            else if plan.entry_name=="one" { Ok(source()) }
            else { Err(Failure::service("HTTP400")) }
        });
        assert!(result.is_ok());
        let mut calls=calls.lock().unwrap().clone(); calls.sort();
        assert_eq!(calls,vec![("one".into(),false),("one".into(),true),("two".into(),false)]);
    }

    #[test]
    fn exhausted_deadline_does_not_send_any_request(){
        let result=resolve_with(plans(),"selected",Instant::now()-Duration::from_secs(1),|_,_,_|panic!("expired playback must not contact a service"));
        assert!(result.is_err());
    }

    #[test]
    fn a_working_entry_is_not_held_by_another_slow_entry(){
        let (release,held)=mpsc::channel();
        let held=Arc::new(Mutex::new(held));
        let both=Arc::new(Barrier::new(2));
        let (done,result)=mpsc::channel();
        let runner=std::thread::spawn(move||{
            let value=resolve_with(plans(),"selected",Instant::now()+Duration::from_secs(3),move|plan,_,_|{
                both.wait();
                if plan.entry_name=="one" { held.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap(); Err(Failure::service("slow failure")) }
                else {Ok(source())}
            });
            done.send(value.is_ok()).unwrap();
        });
        let immediate=result.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        runner.join().unwrap();
        assert_eq!(immediate,Ok(true),"a slow independent route must not delay an already usable video");
    }
}
