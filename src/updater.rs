//! Public GitHub Releases channel. No portal/session credentials enter this client.
use std::{collections::HashSet, ffi::c_void, fs::{self,File}, io::{Read,Write}, path::{Path,PathBuf},
    process::Command, sync::{Mutex,atomic::{AtomicBool,Ordering}}, time::Duration};
use std::os::windows::{fs::MetadataExt,process::CommandExt};
use serde::{Deserialize,Serialize};
use serde_json::{Value,json};
use reqwest::{Url,blocking::Client,redirect::Policy};
use sha2::{Digest,Sha256};

pub const REPOSITORY:&str="john2k2/Nebula";
pub const DOWNLOAD_PAGE:&str="https://github.com/john2k2/Nebula/releases/latest";
const ASSET:&str="Nebula-Windows-portable.zip";
const MAX_ZIP:u64=200*1024*1024;
const MAX_EXPANDED:u64=600*1024*1024;
const ROOTS:[&str;6]=["Nebula.exe","WebView2Loader.dll","LEEME.txt","THIRD-PARTY-NOTICES.txt","licenses","vlc"];
static INSTALLING:AtomicBool=AtomicBool::new(false);
static READY:AtomicBool=AtomicBool::new(false);
static PROGRESS:Mutex<Option<Value>>=Mutex::new(None);

#[derive(Clone,Deserialize,Serialize)]
struct Release { version:String,url:String,digest:Option<String>,size:u64,notes:String }
#[derive(Deserialize,Serialize)]
struct Plan { schema:u32,root:PathBuf,parent:u32,parent_created:u64,old_sha256:String,version:String }

fn version(value:&str)->Option<(u64,u64,u64)>{
    let parts:Vec<_>=value.strip_prefix('v').unwrap_or(value).split('.').collect();
    if parts.len()!=3{return None;}let mut numbers=[0;3];
    for (i,part) in parts.iter().enumerate(){if part.is_empty()||part.len()>9||!part.bytes().all(|v|v.is_ascii_digit())||(part.len()>1&&part.starts_with('0')){return None;}numbers[i]=part.parse().ok()?;}
    Some((numbers[0],numbers[1],numbers[2]))
}
fn trusted_url(url:&Url)->bool{
    url.scheme()=="https"&&url.username().is_empty()&&url.password().is_none()&&url.port_or_known_default()==Some(443)
        &&matches!(url.host_str(),Some("api.github.com"|"github.com"|"release-assets.githubusercontent.com"|"objects.githubusercontent.com"))
}
fn client()->Result<Client,String>{
    Client::builder().user_agent(concat!("Nebula/",env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(240))
        .redirect(Policy::custom(|attempt|if attempt.previous().len()>5||!trusted_url(attempt.url()){attempt.stop()}else{attempt.follow()}))
        .build().map_err(|_|"No se pudo preparar la conexión de actualizaciones.".into())
}
fn parse_release(data:&Value)->Result<Release,String>{
    if data["draft"]==true||data["prerelease"]==true{return Err("La versión no pertenece al canal estable.".into());}
    let tag=data["tag_name"].as_str().ok_or("La versión publicada no tiene una etiqueta válida.")?;
    let numbers=version(tag).ok_or("La versión publicada no tiene un número compatible.")?;
    let canonical=format!("{}.{}.{}",numbers.0,numbers.1,numbers.2);
    if tag!=format!("v{canonical}"){return Err("La etiqueta de actualización no es compatible.".into());}
    let asset=data["assets"].as_array().and_then(|rows|rows.iter().find(|row|row["name"]==ASSET&&row["state"]=="uploaded"))
        .ok_or("La versión todavía no tiene el paquete para Windows.")?;
    let expected=format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{ASSET}");
    if asset["browser_download_url"].as_str()!=Some(&expected){return Err("El paquete no pertenece al canal de Nébula.".into());}
    let size=asset["size"].as_u64().filter(|value|*value>0&&*value<=MAX_ZIP).ok_or("El tamaño de la actualización no es compatible.")?;
    let digest=asset["digest"].as_str().and_then(|value|value.strip_prefix("sha256:"))
        .filter(|value|value.len()==64&&value.bytes().all(|byte|byte.is_ascii_hexdigit())).map(str::to_ascii_lowercase);
    Ok(Release{version:canonical,url:expected,digest,size,notes:data["body"].as_str().unwrap_or("").chars().take(4000).collect()})
}
fn latest()->Result<Option<Release>,String>{
    let response=client()?.get(format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"))
        .timeout(Duration::from_secs(15)).header("Accept","application/vnd.github+json").send()
        .map_err(|_|"No se pudo consultar GitHub. Vuelve a intentarlo más tarde.")?;
    if response.status().as_u16()==404{return Ok(None);}
    if !response.status().is_success(){return Err("GitHub no pudo entregar la última versión. Inténtalo más tarde.".into());}
    let mut bytes=Vec::new();response.take(1024*1024+1).read_to_end(&mut bytes).map_err(|_|"No se pudo leer la versión publicada.")?;
    if bytes.len()>1024*1024{return Err("La respuesta de versiones excede el tamaño permitido.".into());}
    let data:Value=serde_json::from_slice(&bytes).map_err(|_|"GitHub devolvió una respuesta de versiones incompatible.")?;
    parse_release(&data).map(Some)
}
fn newer(release:&Release)->bool{version(&release.version)>version(env!("CARGO_PKG_VERSION"))}
fn phase(name:&str,downloaded:u64,total:u64){if let Ok(mut progress)=PROGRESS.lock(){*progress=Some(json!({"phase":name,"downloaded":downloaded,"total":total}));}}
pub fn ready()->bool{READY.load(Ordering::Acquire)}
pub fn command(command:&str)->Result<Value,String>{
    match command{
        "update.check"=>{let release=latest()?;Ok(match release{
            Some(release)=>json!({"current":env!("CARGO_PKG_VERSION"),"version":release.version,"available":newer(&release),"can_install":release.digest.is_some(),"notes":release.notes,"url":DOWNLOAD_PAGE}),
            None=>json!({"current":env!("CARGO_PKG_VERSION"),"available":false,"unpublished":true,"url":DOWNLOAD_PAGE}),
        })},
        "update.progress"=>Ok(PROGRESS.lock().ok().and_then(|value|value.clone()).unwrap_or(json!({"phase":"idle"}))),
        "update.install"=>{
            if INSTALLING.swap(true,Ordering::AcqRel){return Err("Ya hay una actualización en curso.".into());}
            let result=prepare();if result.is_err(){INSTALLING.store(false,Ordering::Release);phase("error",0,0);}result
        },
        _=>Err("Operación de actualización desconocida.".into()),
    }
}
fn hash(path:&Path)->Result<String,String>{
    let mut file=File::open(path).map_err(|_|"No se pudo leer un archivo de la actualización.")?;
    let mut hasher=Sha256::new();let mut buffer=[0u8;65536];loop{let size=file.read(&mut buffer).map_err(|_|"No se pudo verificar un archivo de la actualización.")?;if size==0{break;}hasher.update(&buffer[..size]);}Ok(hex::encode(hasher.finalize()))
}
fn prepare()->Result<Value,String>{
    phase("checking",0,0);let release=latest()?.filter(newer).ok_or("No hay una versión más nueva para instalar.")?;
    let expected=release.digest.as_deref().ok_or("Esta versión no dispone de verificación de integridad. Descárgala desde GitHub.")?;
    let executable=std::env::current_exe().map_err(|_|"No se pudo localizar Nébula.")?;
    if executable.file_name().and_then(|name|name.to_str())!=Some("Nebula.exe"){return Err("Abre Nebula.exe de la carpeta portable para actualizar desde la aplicación.".into());}
    let root=executable.parent().unwrap().canonicalize().map_err(|_|"No se pudo localizar la carpeta de Nébula.")?;
    let folder=root.join("data/updates").join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&folder).map_err(|_|"La carpeta de Nébula no permite guardar la actualización.")?;
    let archive=folder.join("package.zip");let mut output=File::create(&archive).map_err(|_|"No se pudo guardar la actualización.")?;
    let response=client()?.get(&release.url).header("Accept-Encoding","identity").send().map_err(|_|"La descarga se interrumpió. Puedes volver a intentarlo.")?;
    if !response.status().is_success(){return Err("GitHub no pudo entregar el paquete de actualización.".into());}
    let mut input=response.take(release.size+1);let mut hasher=Sha256::new();let mut bytes=0;let mut buffer=[0u8;65536];phase("downloading",0,release.size);
    loop{let size=input.read(&mut buffer).map_err(|_|"La descarga se interrumpió. Puedes volver a intentarlo.")?;if size==0{break;}bytes+=size as u64;
        if bytes>release.size{return Err("El paquete descargado tiene un tamaño inesperado.".into());}
        output.write_all(&buffer[..size]).map_err(|_|"No hay espacio para guardar la actualización.")?;hasher.update(&buffer[..size]);phase("downloading",bytes,release.size);
    }
    output.sync_all().map_err(|_|"No se pudo terminar de guardar la actualización.")?;drop(output);
    if bytes!=release.size||hex::encode(hasher.finalize())!=expected{return Err("La actualización no pasó la comprobación de integridad. No se modificó la aplicación.".into());}
    phase("verifying",bytes,release.size);extract(&archive,&folder.join("staged"))?;
    let plan=Plan{schema:1,root,parent:std::process::id(),parent_created:process_created(std::process::id())?.ok_or("No se pudo comprobar el proceso actual.")?,old_sha256:hash(&executable)?,version:release.version};
    fs::copy(&executable,folder.join("Nebula-Updater.exe")).map_err(|_|"No se pudo preparar el reinicio de Nébula.")?;
    fs::copy(plan.root.join("WebView2Loader.dll"),folder.join("WebView2Loader.dll")).map_err(|_|"No se pudo preparar el cargador de la actualización.")?;
    let plan_path=folder.join("plan.json");fs::write(&plan_path,serde_json::to_vec(&plan).unwrap()).map_err(|_|"No se pudo preparar la actualización.")?;
    Command::new(folder.join("Nebula-Updater.exe")).arg("--apply-update").arg(&plan_path).current_dir(&folder).creation_flags(0x08000000)
        .spawn().map_err(|_|"No se pudo iniciar el actualizador. No se modificó la aplicación.")?;
    READY.store(true,Ordering::Release);phase("ready",bytes,release.size);Ok(json!({"restart":true,"version":plan.version}))
}
fn allowed_path(name:&str)->Option<PathBuf>{
    if name.len()>1000||name.contains(['\\',':']){return None;}
    let parts:Vec<_>=name.trim_end_matches('/').split('/').collect();
    if parts.is_empty()||parts.iter().any(|part|part.is_empty()||*part=="."||*part==".."||part.ends_with(['.',' '])||part.len()>255){return None;}
    if !ROOTS.contains(&parts[0])||(parts.len()>1&&!matches!(parts[0],"licenses"|"vlc")){return None;}
    for part in &parts{let base=part.split('.').next().unwrap().to_ascii_uppercase();if matches!(base.as_str(),"CON"|"PRN"|"AUX"|"NUL")||["COM","LPT"].iter().any(|prefix|base.strip_prefix(prefix).is_some_and(|suffix|matches!(suffix,"1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9"))){return None;}}
    Some(parts.iter().collect())
}
fn extract(archive:&Path,destination:&Path)->Result<(),String>{
    let file=File::open(archive).map_err(|_|"No se pudo abrir el paquete.")?;
    let mut archive=zip::ZipArchive::new(file).map_err(|_|"El paquete no es un ZIP válido.")?;
    if archive.len()>5000{return Err("El paquete contiene demasiados archivos.".into());}
    fs::create_dir(destination).map_err(|_|"No se pudo preparar la carpeta temporal.")?;
    let mut seen=HashSet::new();let mut total=0u64;
    for index in 0..archive.len(){
        let entry=archive.by_index(index).map_err(|_|"El ZIP contiene un archivo dañado.")?;
        let name=entry.name().map_err(|_|"El ZIP contiene una ruta inválida.")?;
        let relative=allowed_path(&name).ok_or("El paquete contiene rutas ajenas a Nébula.")?;
        if entry.is_symlink()||!seen.insert(relative.to_string_lossy().to_ascii_lowercase()){return Err("El paquete contiene enlaces o archivos duplicados.".into());}
        total=total.checked_add(entry.size()).filter(|size|*size<=MAX_EXPANDED).ok_or("El paquete expandido excede el tamaño permitido.")?;
        let path=destination.join(relative);
        if entry.is_dir(){fs::create_dir_all(path).map_err(|_|"No se pudo preparar una carpeta de actualización.")?;continue;}
        if let Some(parent)=path.parent(){fs::create_dir_all(parent).map_err(|_|"No se pudo preparar la actualización.")?;}
        let size=entry.size();let mut output=File::create(&path).map_err(|_|"No se pudo guardar un archivo de actualización.")?;
        let copied=std::io::copy(&mut entry.take(size+1),&mut output).map_err(|_|"El ZIP no pasó la comprobación de datos.")?;
        if copied!=size{return Err("El ZIP contiene un archivo incompleto.".into());}
    }
    for name in ["Nebula.exe","WebView2Loader.dll","vlc/libvlc.dll","vlc/libvlccore.dll","vlc/plugins/plugins.dat"]{
        if !destination.join(name).is_file(){return Err("El paquete no incluye todos los componentes de Nébula.".into());}
    }
    let mut signature=[0;2];File::open(destination.join("Nebula.exe")).and_then(|mut file|file.read_exact(&mut signature)).map_err(|_|"El ejecutable del paquete está incompleto.")?;
    if &signature!=b"MZ"{return Err("El paquete no contiene un ejecutable de Windows válido.".into());}Ok(())
}

#[link(name="kernel32")]
unsafe extern "system"{
    fn OpenProcess(access:u32,inherit:i32,pid:u32)->*mut c_void;
    fn GetProcessTimes(handle:*mut c_void,created:*mut u64,exit:*mut u64,kernel:*mut u64,user:*mut u64)->i32;
    fn WaitForSingleObject(handle:*mut c_void,milliseconds:u32)->u32;
    fn CloseHandle(handle:*mut c_void)->i32;
}
fn process_created(pid:u32)->Result<Option<u64>,String>{
    let handle=unsafe{OpenProcess(0x1000,0,pid)};if handle.is_null(){return Ok(None);}let(mut created,mut exit,mut kernel,mut user)=(0,0,0,0);
    let ok=unsafe{GetProcessTimes(handle,&mut created,&mut exit,&mut kernel,&mut user)};unsafe{CloseHandle(handle);}
    if ok==0{Err("No se pudo comprobar el proceso de Nébula.".into())}else{Ok(Some(created))}
}
fn wait_parent(plan:&Plan)->Result<(),String>{
    let handle=unsafe{OpenProcess(0x00100000|0x1000,0,plan.parent)};if handle.is_null(){return Ok(());}
    let(mut created,mut exit,mut kernel,mut user)=(0,0,0,0);let ok=unsafe{GetProcessTimes(handle,&mut created,&mut exit,&mut kernel,&mut user)};
    let result=if ok==0{Err("No se pudo comprobar el proceso anterior.".into())}else if created!=plan.parent_created{Ok(())}
        else if unsafe{WaitForSingleObject(handle,60000)}==0{Ok(())}else{Err("Nébula no terminó a tiempo. Se conservó la versión anterior.".into())};unsafe{CloseHandle(handle);}result
}
fn commit(root:&Path,stage:&Path,backup:&Path,mut checkpoint:impl FnMut(usize)->Result<(),String>)->Result<(),String>{
    fs::create_dir(backup).map_err(|_|"No se pudo crear la copia de recuperación.")?;
    let mut changed:Vec<(&str,bool)>=Vec::new();
    let result:Result<(),String>=(||{for (index,name) in ROOTS.iter().enumerate(){
        let source=stage.join(name);if !source.exists(){continue;}let target=root.join(name);let old=target.exists();
        if old{let metadata=fs::symlink_metadata(&target).map_err(|_|"No se pudo comprobar la instalación.")?;
            if metadata.file_attributes()&0x400!=0{return Err("La instalación contiene enlaces de carpetas incompatibles con la actualización.".into());}
            fs::rename(&target,backup.join(name)).map_err(|_|"Un archivo sigue abierto. Se conservará la versión anterior.")?;}
        changed.push((name,old));checkpoint(index)?;
        fs::rename(source,&target).map_err(|_|"No se pudo reemplazar un componente de Nébula.".to_string())?;
    }Ok(())})();
    if let Err(error)=result{
        let mut restored=true;for(name,old)in changed.into_iter().rev(){let target=root.join(name);
            if target.exists()&&fs::rename(&target,stage.join(name)).is_err(){restored=false;}
            if old&&fs::rename(backup.join(name),&target).is_err(){restored=false;}}
        return Err(if restored{format!("{error} Se restauró la versión anterior.")}else{format!("{error} La copia de recuperación se conservó junto al actualizador.")});
    }Ok(())
}
pub fn apply(plan_path:&Path)->Result<(),String>{
    let helper=std::env::current_exe().map_err(|_|"No se pudo localizar el actualizador.")?;
    let folder=helper.parent().unwrap().canonicalize().map_err(|_|"No se pudo comprobar la carpeta del actualizador.")?;
    if plan_path.canonicalize().ok()!=Some(folder.join("plan.json")){return Err("El plan no pertenece a este actualizador.".into());}
    let plan:Plan=serde_json::from_slice(&fs::read(plan_path).map_err(|_|"No se pudo leer el plan de actualización.")?).map_err(|_|"El plan de actualización está dañado.")?;
    let root=plan.root.canonicalize().map_err(|_|"No se pudo localizar la instalación.")?;
    if plan.schema!=1||folder.parent().and_then(Path::parent)!=Some(root.join("data").as_path())||plan.old_sha256.len()!=64||version(&plan.version)<=version(env!("CARGO_PKG_VERSION")){
        return Err("El plan de actualización no pertenece a esta instalación.".into());}
    wait_parent(&plan)?;
    let instance=crate::instance::Instance::acquire().map_err(|_|"No se pudo comprobar si Nébula está abierto.")?.ok_or("Nébula se abrió de nuevo antes de terminar la actualización.")?;
    if hash(&root.join("Nebula.exe"))?!=plan.old_sha256{return Err("La instalación cambió desde que se preparó la actualización.".into());}
    let result=commit(&root,&folder.join("staged"),&folder.join("backup"),|_|Ok(()));
    let report=json!({"version":plan.version,"success":result.is_ok(),"message":result.as_ref().err()});
    let _=fs::write(root.join("data/update-result.json"),serde_json::to_vec_pretty(&report).unwrap());
    drop(instance);
    // Restart the new version after success, or the restored version on failure.
    let _=Command::new(root.join("Nebula.exe")).current_dir(&root).spawn();result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn folder()->PathBuf { let p=std::env::temp_dir().join(format!("nebula-update-{}",uuid::Uuid::new_v4()));fs::create_dir(&p).unwrap();p }
    fn metadata()->Value { json!({"tag_name":"v0.6.0","assets":[{"name":ASSET,"state":"uploaded","size":1234,"digest":format!("sha256:{}","ab".repeat(32)),"browser_download_url":format!("https://github.com/{REPOSITORY}/releases/download/v0.6.0/{ASSET}")}]}) }
    #[test]
    fn versions_and_public_channel_are_strict(){
        assert!(version("0.10.0")>version("0.9.9"));
        for invalid in ["1.2","01.2.3","1.2.3-beta","1.2.-1","1.2.3.4"]{assert!(version(invalid).is_none());}
        assert!(parse_release(&metadata()).is_ok());
        let mut v=metadata();v["assets"][0]["browser_download_url"]=json!("https://example.com/package.zip");assert!(parse_release(&v).is_err());
        let mut v=metadata();v["prerelease"]=json!(true);assert!(parse_release(&v).is_err());
        let mut v=metadata();v["assets"][0]["digest"]=Value::Null;assert!(parse_release(&v).unwrap().digest.is_none());
        for u in ["http://github.com/a","https://github.com.evil.test/a","https://user@github.com/a","https://github.com:8443/a"]{assert!(!trusted_url(&Url::parse(u).unwrap()));}
    }
    #[test]
    fn package_paths_cannot_replace_personal_data_or_escape(){
        for path in ["../Nebula.exe","/Nebula.exe","data/session.json","vlc/../../data/a","vlc\\a.dll","vlc/C:/a","vlc/CON.txt","vlc/a. ","vlc//a"] {assert!(allowed_path(path).is_none(),"{path}");}
        assert!(allowed_path("vlc/plugins/codec/libavcodec_plugin.dll").is_some());
    }
    #[test]
    fn failed_commit_restores_every_component_and_preserves_data(){
        let p=folder();let root=p.join("app");let stage=p.join("staged");fs::create_dir(&root).unwrap();fs::create_dir(&stage).unwrap();fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/session.json"),b"personal").unwrap();
        for name in ["Nebula.exe","WebView2Loader.dll"] {fs::write(root.join(name),b"old").unwrap();fs::write(stage.join(name),b"new").unwrap();}
        let result=commit(&root,&stage,&p.join("backup"),|index|if index==1 {Err("simulated locked file".into())}else{Ok(())});assert!(result.is_err());
        for name in ["Nebula.exe","WebView2Loader.dll"]{assert_eq!(fs::read(root.join(name)).unwrap(),b"old");assert_eq!(fs::read(stage.join(name)).unwrap(),b"new");}
        assert_eq!(fs::read(root.join("data/session.json")).unwrap(),b"personal");
        commit(&root,&stage,&p.join("backup2"),|_|Ok(())).unwrap();assert_eq!(fs::read(root.join("Nebula.exe")).unwrap(),b"new");assert_eq!(fs::read(root.join("data/session.json")).unwrap(),b"personal");fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn zip_rejects_escape_and_missing_runtime(){
        use zip::{ZipWriter,write::SimpleFileOptions};
        let p=folder();let archive=p.join("bad.zip");let mut z=ZipWriter::new(File::create(&archive).unwrap());z.start_file("data/session.json",SimpleFileOptions::default()).unwrap();z.write_all(b"invalid").unwrap();z.finish().unwrap();
        assert!(extract(&archive,&p.join("bad")).is_err());assert!(!p.join("data/session.json").exists());
        let mut z=ZipWriter::new(File::create(&archive).unwrap());z.start_file("Nebula.exe",SimpleFileOptions::default()).unwrap();z.write_all(b"MZ").unwrap();z.finish().unwrap();assert!(extract(&archive,&p.join("incomplete")).is_err());fs::remove_dir_all(p).unwrap();
    }
    #[test]
    #[ignore="Requires the compiled distribution archive"]
    fn distribution_archive_extracts_with_the_actual_updater(){
        let p=folder();extract(&Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/Nebula-Windows-portable.zip"),&p.join("staged")).unwrap();fs::remove_dir_all(p).unwrap();
    }
}

