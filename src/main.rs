#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod desktop;
mod media_proxy;
mod api;
mod models;
mod protocol;
mod ranger;
mod ranger_identity;
#[cfg(test)] mod cast;
mod instance;
mod playback_diagnostics;
mod vod_resolver;
mod subtitles;
mod native_player;
mod image_cache;
mod updater;
mod native_cast;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--apply-update") {
        if let Some(path) = arguments.get(1) {
            if let Err(error) = updater::apply(std::path::Path::new(path)) { startup_message(&error); }
        }
        return;
    }
    if arguments.iter().any(|arg| arg == "--webview-version") {
        match wry::webview_version() {
            Ok(version) => println!("WebView2 {version}"),
            Err(error) => {
                eprintln!("No se encontró WebView2: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    let smoke = arguments.iter().any(|arg| arg == "--smoke-test");
    let offline = smoke || arguments.iter().any(|arg| arg == "--offline-ui");
    let _instance = if offline { None } else {
        match instance::Instance::acquire() {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                eprintln!("Nébula ya está abierto. Se usará la ventana existente.");
                instance::show_existing();
                return;
            }
            Err(_) => {
                startup_message("No se pudo comprobar si Nébula ya está abierto. Cierra las otras ventanas de Nébula e inténtalo de nuevo.");
                return;
            }
        }
    };
    if let Some(index) = arguments.iter().position(|arg| arg == "--probe") {
        let destination = arguments.get(index + 1).filter(|arg| !arg.starts_with("--"));
        let report = probe();
        let serialized = serde_json::to_string_pretty(&report).expect("probe summary is JSON");
        if let Some(path) = destination {
            if std::fs::write(path, &serialized).is_err() {
                eprintln!("No se pudo guardar el informe de conexión.");
                std::process::exit(2);
            }
        }
        println!("{serialized}");
        std::process::exit(if report["ok"] == true { 0 } else { 1 });
    }
    let handler = move |command: &str, params: &serde_json::Value| {
        if offline { unavailable(command, params) } else { api::handle(command, params) }
    };
    if let Err(error) = desktop::run(handler, smoke) {
        eprintln!("No se pudo iniciar Nébula: {error}");
        if let Ok(path) = desktop::data_dir() {
            let _ = std::fs::create_dir_all(&path);
            let _ = std::fs::write(path.join("startup-error.txt"), error.to_string());
        }
        if !smoke {
            startup_message(&format!("No se pudo iniciar Nébula.\n\n{error}\n\nComprueba que WebView2 Runtime esté disponible y que WebView2Loader.dll acompañe al ejecutable."));
        }
        std::process::exit(1);
    }
}

fn unavailable(_command: &str, _params: &serde_json::Value) -> Result<serde_json::Value, String> {
    Err("La conexión está desactivada en este modo de comprobación local.".into())
}

fn probe() -> serde_json::Value {
    use serde_json::json;
    use std::time::Instant;
    let began = Instant::now();
    let mut steps = Vec::new();
    let mut connected = false;
    let mut successful = false;
    let started = Instant::now();
    match api::handle("status", &json!({})) {
        Ok(status) => {
            connected = status["connected"] == true;
            steps.push(json!({"command":"status", "ok":true, "connected":connected, "elapsed_ms":started.elapsed().as_millis()}));
            if connected {
                let started = Instant::now();
                match api::handle("categories", &json!({"kind":"vod"})) {
                    Ok(categories) => {
                        let count = categories.as_array().map_or(0, Vec::len);
                        steps.push(json!({"command":"categories", "ok":true, "count":count, "elapsed_ms":started.elapsed().as_millis()}));
                        if let Some(category) = categories.as_array().and_then(|values| values.first()).and_then(|value| value.get("id")) {
                            let started = Instant::now();
                            match api::handle("catalog", &json!({"kind":"vod", "category":category, "page":1})) {
                                Ok(catalog) => {
                                    successful = true;
                                    steps.push(json!({"command":"catalog", "ok":true, "count":catalog["items"].as_array().map_or(0, Vec::len), "page":catalog["page"].as_u64(), "has_more":catalog["has_more"].as_bool(), "elapsed_ms":started.elapsed().as_millis()}));
                                }
                                Err(error) => steps.push(probe_failure("catalog", started.elapsed().as_millis(), &error)),
                            }
                        } else {
                            successful = true;
                            steps.push(json!({"command":"catalog", "skipped":true, "reason":"no_categories"}));
                        }
                    }
                    Err(error) => steps.push(probe_failure("categories", started.elapsed().as_millis(), &error)),
                }
            }
        }
        Err(error) => steps.push(probe_failure("status", started.elapsed().as_millis(), &error)),
    }
    json!({"ok":successful, "connected":connected, "elapsed_ms":began.elapsed().as_millis(), "steps":steps})
}

fn probe_failure(command: &str, elapsed_ms: u128, error: &str) -> serde_json::Value {
    use serde_json::json;
    let operation = ["snToken", "active", "getAuthInfo", "getSlbInfo", "getColumnContents", "getShelveData", "getLiveData", "searchByName", "getItemData", "startPlayVOD"]
        .into_iter().find(|name| error.contains(name));
    let return_code = error.split("(código ").nth(1).and_then(|value| value.split(')').next())
        .filter(|value| value.len() <= 12 && value.chars().all(|c| c.is_ascii_digit() || c == '-'));
    let http_status = error.split("HTTP ").nth(1).and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u16>().ok()).filter(|value| (100..600).contains(value));
    // A probe never copies arbitrary service diagnostics, URLs, identifiers or titles.
    let local_protocol_errors = [
        "La configuración de compatibilidad está dañada.",
        "El APK no contiene un servidor de catálogo válido.",
        "La clave de compatibilidad no tiene un formato válido.",
        "La clave de compatibilidad tiene una longitud inválida.",
        "No se pudo iniciar el cifrado del protocolo.",
        "El servidor configurado tiene una dirección inválida.",
        "El servidor configurado tiene una dirección incompatible.",
        "La identidad local de instalación es inválida. Se conservó el archivo sin modificarlo.",
        "Operación ausente en el contrato del APK.",
    ];
    let (code, message) = if local_protocol_errors.contains(&error) {
        ("local_protocol_error", error)
    } else if error.starts_with("El servicio rechazó") {
        ("service_rejected", "El servicio rechazó la operación.")
    } else if http_status.is_some() {
        ("http_error", "El servidor devolvió un error HTTP.")
    } else if error.contains("tardó demasiado") {
        ("timeout", "El servidor tardó demasiado en responder.")
    } else if error.contains("conectar") || error.contains("conexión segura") {
        ("connection_error", "No se pudo conectar de forma segura.")
    } else if error.contains("archivo") || error.contains("carpeta") {
        ("local_data_error", "No se pudieron preparar los datos locales.")
    } else {
        ("service_error", "No se pudo completar la operación.")
    };
    json!({"command":command, "ok":false, "elapsed_ms":elapsed_ms, "error":{"code":code, "message":message, "operation":operation, "return_code":return_code, "http_status":http_status}})
}

#[cfg(windows)]
fn startup_message(message: &str) {
    use std::{ffi::c_void, iter::once};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(window: *mut c_void, text: *const u16, caption: *const u16, flags: u32) -> i32;
    }
    let message: Vec<u16> = message.encode_utf16().chain(once(0)).collect();
    let title: Vec<u16> = "Nébula".encode_utf16().chain(once(0)).collect();
    // Both strings remain alive until the synchronous Windows dialog returns.
    unsafe { MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), 0x10); }
}

#[cfg(not(windows))]
fn startup_message(_message: &str) {}

