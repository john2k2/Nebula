//! Persistent, bounded raster cache. Only artwork returned by the backend is
//! registered; a browser request supplies an opaque key, never a destination URL.
use std::{collections::HashMap, fs, io::Read, path::PathBuf, sync::{Arc, Mutex, mpsc, atomic::{AtomicUsize,Ordering}}, time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde_json::Value;
use md5::{Md5,Digest};
use wry::{RequestAsyncResponder,http::Response};

const MAX_IMAGE:u64=8*1024*1024;
const MAX_DISK:u64=256*1024*1024;
const MAX_FILES:usize=2000;
const MAX_AGE:Duration=Duration::from_secs(30*86400);
struct Shared {
    directory:PathBuf, urls:Mutex<HashMap<String,Url>>,
    clients:Mutex<HashMap<String,(Client,Instant)>>,
    stripes:[Mutex<()>;16], maintenance:Mutex<()>,
    hits:AtomicUsize, downloads:AtomicUsize, failures:AtomicUsize,
}
struct Job { key:String, responder:RequestAsyncResponder }
#[derive(Clone)] pub struct ImageCache { shared:Arc<Shared>, sender:mpsc::SyncSender<Job> }

fn valid_key(key:&str)->bool { key.len()==32 && key.bytes().all(|b|b.is_ascii_hexdigit()) }
fn raster(bytes:&[u8])->Option<&'static str>{
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n"){Some("image/png")}
    else if bytes.starts_with(b"\xff\xd8\xff"){Some("image/jpeg")}
    else if bytes.starts_with(b"GIF87a")||bytes.starts_with(b"GIF89a"){Some("image/gif")}
    else if bytes.len()>=12 && &bytes[..4]==b"RIFF" && &bytes[8..12]==b"WEBP"{Some("image/webp")}
    else if bytes.len()>=16 && &bytes[4..8]==b"ftyp" && matches!(&bytes[8..12],b"avif"|b"avis"){Some("image/avif")}
    else{None}
}
impl ImageCache {
    pub fn new(directory:PathBuf)->std::io::Result<Self>{
        fs::create_dir_all(&directory)?;
        let shared=Arc::new(Shared{directory,urls:Mutex::new(HashMap::new()),clients:Mutex::new(HashMap::new()),
            stripes:std::array::from_fn(|_|Mutex::new(())),maintenance:Mutex::new(()),
            hits:AtomicUsize::new(0),downloads:AtomicUsize::new(0),failures:AtomicUsize::new(0)});
        shared.trim();
        let (sender,receiver)=mpsc::sync_channel::<Job>(128);
        let receiver=Arc::new(Mutex::new(receiver));
        for index in 0..4 {
            let shared=shared.clone();let receiver=receiver.clone();
            std::thread::Builder::new().name(format!("nebula-art-{index}")).spawn(move||loop{
                let job=match receiver.lock().map(|receiver|receiver.recv()){Ok(Ok(job))=>job,_=>break};
                let result=shared.load(&job.key);
                let response=match result {
                    Ok(bytes)=>Response::builder().status(200).header("Content-Type",raster(&bytes).unwrap())
                        .header("Cache-Control","private, max-age=86400").header("X-Content-Type-Options","nosniff")
                        .header("Referrer-Policy","no-referrer").body(bytes).unwrap(),
                    Err(())=>{shared.failures.fetch_add(1,Ordering::Relaxed);Response::builder().status(404)
                        .header("Cache-Control","no-store").body(Vec::new()).unwrap()},
                };
                job.responder.respond(response);
            })?;
        }
        Ok(Self{shared,sender})
    }
    pub fn rewrite(&self,value:&mut Value){
        match value {
            Value::Array(rows)=>for row in rows{self.rewrite(row)},
            Value::Object(object)=>for (name,value) in object {
                if matches!(name.as_str(),"poster"|"backdrop") {
                    if let Some(url)=value.as_str().and_then(|s|Url::parse(s).ok()) {
                        if matches!(url.scheme(),"http"|"https") && url.username().is_empty() && url.password().is_none()
                            && url.host_str()!=Some("xuper.localhost") && url.as_str().len()<=8192 {
                            let key=hex::encode(Md5::digest(url.as_str().as_bytes()));
                            if let Ok(mut urls)=self.shared.urls.lock(){
                                if urls.len()<8192 || urls.contains_key(&key){
                                    urls.insert(key.clone(),url);*value=Value::String(format!("http://xuper.localhost/images/{key}"));
                                }
                            }
                        }
                    }
                }else{self.rewrite(value)}
            },
            _=>{},
        }
    }
    pub fn serve(&self,path:&str,responder:RequestAsyncResponder){
        let key=path.strip_prefix("/images/").unwrap_or_default();
        if !valid_key(key){responder.respond(Response::builder().status(404).body(Vec::new()).unwrap());return;}
        if let Err(error)=self.sender.try_send(Job{key:key.to_string(),responder}){
            let job=match error{mpsc::TrySendError::Full(job)|mpsc::TrySendError::Disconnected(job)=>job};
            job.responder.respond(Response::builder().status(503).header("Cache-Control","no-store").body(Vec::new()).unwrap());
        }
    }
    pub fn report(&self){
        let report=serde_json::json!({"disk_hits":self.shared.hits.load(Ordering::Relaxed),
            "downloads":self.shared.downloads.load(Ordering::Relaxed),"failures":self.shared.failures.load(Ordering::Relaxed),
            "limit_bytes":MAX_DISK,"limit_files":MAX_FILES,"max_age_days":30});
        if let Ok(bytes)=serde_json::to_vec_pretty(&report){let _=fs::write(self.shared.directory.join("statistics.json"),bytes);}
    }
}
impl Shared {
    fn cached(&self,key:&str)->Option<Vec<u8>>{
        let path=self.directory.join(format!("{key}.img"));
        let metadata=fs::metadata(&path).ok()?;
        if metadata.len()>MAX_IMAGE || metadata.modified().ok()?.elapsed().ok()? > MAX_AGE{return None;}
        let bytes=fs::read(path).ok()?;raster(&bytes)?;Some(bytes)
    }
    fn load(&self,key:&str)->Result<Vec<u8>,()>{
        if !valid_key(key){return Err(());}
        let stripe=usize::from_str_radix(&key[..1],16).map_err(|_|())?;
        let _guard=self.stripes[stripe].lock().map_err(|_|())?;
        if let Some(bytes)=self.cached(key){self.hits.fetch_add(1,Ordering::Relaxed);return Ok(bytes);}
        let mut url=self.urls.lock().map_err(|_|())?.get(key).cloned().ok_or(())?;
        let deadline=Instant::now()+Duration::from_secs(12);
        for _ in 0..6 {
            let origin=url.origin().ascii_serialization();
            let cached=self.clients.lock().map_err(|_|())?.get(&origin).filter(|(_,expiry)|*expiry>Instant::now()).map(|(client,_)|client.clone());
            let client=if let Some(client)=cached{client}else{
                // Resolver pins public addresses, preserving TLS and rejecting
                // loopback/private redirects. Artwork carries no portal headers.
                let (builder,ttl)=crate::protocol::media_client_builder_until(&url,Some(deadline)).map_err(|_|())?;
                let client=builder.redirect(Policy::none()).build().map_err(|_|())?;
                let mut clients=self.clients.lock().map_err(|_|())?;
                clients.retain(|_,(_,expiry)|*expiry>Instant::now());
                if clients.len()>=32{clients.clear();}
                clients.insert(origin,(client.clone(),Instant::now()+ttl.min(Duration::from_secs(300))));client
            };
            let timeout=deadline.checked_duration_since(Instant::now()).ok_or(())?;
            let response=client.get(url.clone()).timeout(timeout).header("Accept","image/avif,image/webp,image/*")
                .header("Accept-Encoding","identity").send().map_err(|_|())?;
            if matches!(response.status().as_u16(),301|302|303|307|308){
                let location=response.headers().get("Location").and_then(|v|v.to_str().ok()).ok_or(())?;
                url=url.join(location).map_err(|_|())?;continue;
            }
            if !response.status().is_success() || response.content_length().is_some_and(|size|size>MAX_IMAGE){return Err(());}
            let mut bytes=Vec::new();response.take(MAX_IMAGE+1).read_to_end(&mut bytes).map_err(|_|())?;
            if bytes.len() as u64>MAX_IMAGE || raster(&bytes).is_none(){return Err(());}
            let path=self.directory.join(format!("{key}.img"));
            let temporary=self.directory.join(format!("{key}.tmp"));
            if fs::write(&temporary,&bytes).is_ok(){
                // Only this key's worker can replace its expired file.
                if path.is_file(){let _=fs::remove_file(&path);}
                let _=fs::rename(&temporary,&path);
            }
            let count=self.downloads.fetch_add(1,Ordering::Relaxed)+1;
            if count.is_multiple_of(16){self.trim();}
            return Ok(bytes);
        }
        Err(())
    }
    fn trim(&self){
        let Ok(_guard)=self.maintenance.lock()else{return;};
        let Ok(entries)=fs::read_dir(&self.directory)else{return;};
        let mut entries:Vec<_>=entries.flatten().filter_map(|entry|{
            let name=entry.file_name();let name=name.to_str()?;
            let key=name.strip_suffix(".img")?;if !valid_key(key){return None;}
            let meta=entry.metadata().ok()?;if !meta.is_file(){return None;}
            Some((entry.path(),meta.len(),meta.modified().unwrap_or(UNIX_EPOCH)))
        }).collect();
        entries.sort_by_key(|entry|entry.2);
        let mut bytes:u64=entries.iter().map(|entry|entry.1).sum();let mut count=entries.len();
        for (path,size,modified) in entries{
            if bytes>MAX_DISK || count>MAX_FILES || SystemTime::now().duration_since(modified).unwrap_or_default()>MAX_AGE{
                if fs::remove_file(path).is_ok(){bytes=bytes.saturating_sub(size);count=count.saturating_sub(1);}
            }
        }
    }
}

#[cfg(test)] mod tests{
    use super::*;use serde_json::json;
    fn cache()->ImageCache{let directory=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/image-cache-tests").join(uuid::Uuid::new_v4().simple().to_string());ImageCache::new(directory).unwrap()}
    #[test] fn registered_artwork_is_local_and_other_urls_are_unchanged(){
        let cache=cache();let mut item=json!({"poster":"https://art.example/a.jpg","backdrop":"https://art.example/b.png","url":"https://video.example/movie","episodes":[{"poster":"https://art.example/a.jpg"}]});
        cache.rewrite(&mut item);assert!(item["poster"].as_str().unwrap().starts_with("http://xuper.localhost/images/"));
        assert_eq!(item["poster"],item["episodes"][0]["poster"]);assert_eq!(item["url"],"https://video.example/movie");
        let original=item.clone();cache.rewrite(&mut item);assert_eq!(item,original);
    }
    #[test] fn disk_hit_survives_new_cache_instance_without_network_registry(){
        let cache=cache();let key="a".repeat(32);let bytes=b"\x89PNG\r\n\x1a\nsynthetic-raster";
        fs::write(cache.shared.directory.join(format!("{key}.img")),bytes).unwrap();
        let reopened=ImageCache::new(cache.shared.directory.clone()).unwrap();
        assert_eq!(reopened.shared.load(&key).unwrap(),bytes);assert_eq!(reopened.shared.hits.load(Ordering::Relaxed),1);assert_eq!(reopened.shared.downloads.load(Ordering::Relaxed),0);
    }
    #[test] fn invalid_paths_and_active_or_corrupt_formats_are_not_cache_hits(){
        let cache=cache();assert!(cache.shared.load("../../session.json").is_err());assert!(raster(b"<svg onload='x' />").is_none());
        let key="b".repeat(32);fs::write(cache.shared.directory.join(format!("{key}.img")),b"<html>failure</html>").unwrap();assert!(cache.shared.load(&key).is_err());
    }
    #[test] fn stale_art_is_expired_and_cleaned_without_touching_other_files(){
        let cache=cache();let path=cache.shared.directory.join(format!("{}.img","c".repeat(32)));
        fs::write(&path,b"\xff\xd8\xfftest").unwrap();fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(UNIX_EPOCH).unwrap();
        let protected=cache.shared.directory.join("keep.txt");fs::write(&protected,b"preserve").unwrap();
        assert!(cache.shared.cached(&"c".repeat(32)).is_none());cache.shared.trim();assert!(!path.exists());assert!(protected.is_file());
    }
}
