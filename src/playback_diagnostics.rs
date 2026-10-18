//! Opt-in network metadata for comparing the connected Android app and desktop.
//! Never records the auth query, request headers, bodies, credentials or errors.
use serde_json::{json, Value};
use std::{fs::OpenOptions, io::Write, sync::Mutex, time::{SystemTime, UNIX_EPOCH}};

static OUTPUT_LOCK: Mutex<()> = Mutex::new(());

fn record(mut value: Value) {
    if std::env::var("XUPER_PLAYBACK_DIAGNOSTIC").as_deref() != Ok("1") { return; }
    let Some(directory) = std::env::var_os("XUPER_DIAGNOSTIC_DIR") else { return; };
    let directory = std::path::PathBuf::from(directory);
    if !directory.is_absolute() || !directory.is_dir() { return; }
    value["observed_at_ms"] = json!(SystemTime::now().duration_since(UNIX_EPOCH).map(|time|time.as_millis()).unwrap_or_default());
    let Ok(_lock) = OUTPUT_LOCK.lock() else { return; };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(directory.join("playback-network.safe.jsonl")) else { return; };
    if let Ok(mut line) = serde_json::to_vec(&value) { line.push(b'\n'); let _ = file.write_all(&line); }
}

/// Optional debug oracle: private inputs cross an anonymous pipe only. The
/// helper executes the original isolated codec and returns equality booleans.
#[cfg(debug_assertions)]
pub fn verify_vod_plan(plan: &crate::ranger::SlbRequest) {
    use std::{process::{Command, Stdio}, time::{Duration, Instant}};
    if std::env::var("XUPER_PLAYBACK_DIAGNOSTIC").as_deref() != Ok("1") { return; }
    let (Some(python), Some(script)) = (std::env::var_os("XUPER_CODEC_PYTHON"), std::env::var_os("XUPER_CODEC_ORACLE")) else { return; };
    let Ok(constants) = serde_json::from_str::<Value>(include_str!("../analysis/protocol.constants.local.json")) else { return; };
    let mut plaintext = serde_json::to_string(&plan.authorizations).unwrap_or_default();
    plaintext.push('\n');
    let wire = plan.url.query().and_then(|query| query.strip_prefix("auth=")).unwrap_or_default();
    let input = json!({"version":11,"communication_key":constants["request_key"],"plaintext":plaintext,"expected_wire":wire,"url":plan.url.as_str()});
    let mut command = Command::new(python);
    command.arg(script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    let Ok(mut child) = command.spawn() else { return; };
    if let Some(mut stdin) = child.stdin.take() { let _ = serde_json::to_writer(&mut stdin, &input); }
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => { let _ = child.kill(); let _ = child.wait(); record(json!({"event":"native_codec_oracle","completed":false})); return; }
        }
    }
    if let Ok(output) = child.wait_with_output() {
        if let Ok(result) = serde_json::from_slice::<Value>(&output.stdout) {
            record(json!({"event":"native_codec_oracle","host":plan.url.host_str(),
                "completed":result["completed"].as_bool(),"wire_matches":result["wire_matches"].as_bool(),
                "plaintext_matches":result["plaintext_matches"].as_bool(),"instructions":result["instructions"].as_u64(),
                "phone_curl":result["phone_curl"],"windows_curl":result["windows_curl"]}));
        }
    }
}

pub fn record_plan(url: &reqwest::Url) {
    record(json!({"event":"slb_request", "scheme":url.scheme(), "host":url.host_str(),
        "port":url.port_or_known_default(), "path":url.path(),
        "query_bytes":url.query().map_or(0,str::len)}));
}

pub fn record_bootstrap(value: &Value) {
    if std::env::var("XUPER_PLAYBACK_DIAGNOSTIC").as_deref() != Ok("1") { return; }
    fn parameter<'a>(text: &'a str, name: &str) -> &'a str {
        let marker = format!("{name}=");
        text.rfind(&marker).map(|offset| text[offset + marker.len()..].split('&').next().unwrap_or_default()).unwrap_or_default()
    }
    let entries: Vec<_> = value["cdn_list"].as_array().into_iter().flatten().map(|entry| {
        let auths: Vec<_> = entry["url_list"].as_array().into_iter().flatten().filter_map(|item| item["url"].as_str()).map(|auth| {
            let link = parameter(auth,"link");
            let parsed = reqwest::Url::parse(link).ok();
            let keys: Vec<_> = auth.split('&').filter_map(|part| part.split_once('=').map(|(key,_)|key)).filter(|key|key.len()<40 && key.chars().all(|c|c.is_ascii_alphanumeric()||c=='_')).collect();
            json!({"bytes":auth.len(),"keys":keys,"link_bytes":link.len(),"link_scheme":parsed.as_ref().map(|url|url.scheme()),
                "link_host":parsed.as_ref().and_then(|url|url.host_str()),"link_path_segments":parsed.as_ref().and_then(|url|url.path_segments().map(|parts|parts.count())),
                "sign_type_bytes":parameter(auth,"sign_type").len()})
        }).collect();
        let endpoint = entry["main_addr"].as_str().and_then(|raw|reqwest::Url::parse(raw).ok());
        json!({"business":entry["tag"],"cdn_type":entry["cdn_type"],"main_scheme":endpoint.as_ref().map(|url|url.scheme()),"main_host":endpoint.as_ref().and_then(|url|url.host_str()),
            "main_bytes":entry["main_addr"].as_str().map(str::len),"auths":auths})
    }).collect();
    let params = value["play_params"].as_str().unwrap_or_default();
    let controls: Vec<_> = ["star_proxy","transmit_protocol","vod_min_peers","p2p_main_addr","ice_main_addr"].into_iter().map(|name| {
        let raw = parameter(params,name);
        let parsed = reqwest::Url::parse(raw).ok();
        json!({"name":name,"bytes":raw.len(),"number":raw.parse::<i32>().ok(),
            "scheme":parsed.as_ref().map(|url|url.scheme()),"host":parsed.as_ref().and_then(|url|url.host_str())})
    }).collect();
    record(json!({"event":"vod_bootstrap_shape","entries":entries,"controls":controls}));
}

/// Option availability only: no content IDs, licenses or authorization tags.
pub fn record_vod_options(play: &Value, slb: &Value) {
    if std::env::var("XUPER_PLAYBACK_DIAGNOSTIC").as_deref() != Ok("1") { return; }
    let entry=slb["cdn_list"].as_array().into_iter().flatten()
        .filter(|entry|entry["tag"].as_str()==Some("vod"))
        .min_by_key(|entry|crate::models::integer(entry,"serial_number").unwrap_or(0));
    let tags:Vec<_>=entry.into_iter().flat_map(|entry|entry["url_list"].as_array().into_iter().flatten())
        .filter(|row|row["url"].as_str().is_some_and(|url|!url.is_empty()))
        .filter_map(|row|row["tag"].as_str()).collect();
    let episodes:Vec<_>=play["episodeList"].as_array().into_iter().flatten().take(64).map(|episode|{
        let groups:Vec<_>=episode["totalMovieList"].as_array().into_iter().flatten().take(16).map(|group|{
            let movies:Vec<_>=group["movieList"].as_array().into_iter().flatten().take(64).map(|movie|{
                let first=movie["licenseList"].as_array().and_then(|rows|rows.first());
                let languages:Vec<_>=movie["audioInfo"].as_str().unwrap_or_default().split(',')
                    .filter(|language|crate::models::valid_audio_language(language)).take(16).collect();
                let safe_format=|name:&str|movie[name].as_str().filter(|format|matches!(*format,
                    "mp4"|"ts"|"m3u8"|"hls"|"h264"|"h265"|"avc"|"hevc"|"H264"|"H265"|"AVC"|"HEVC"));
                json!({"license_count":movie["licenseList"].as_array().map_or(0,Vec::len),
                    "first_license_present":first.is_some(),
                    "first_license_nonempty":first.and_then(|row|row["license"].as_str()).is_some_and(|value|!value.is_empty()),
                    "first_tag_allowed":first.and_then(|row|row["tag"].as_str()).is_some_and(|tag|tags.contains(&tag)),
                    "audio_languages":languages,"video_format":safe_format("videoFormat"),"encode_format":safe_format("encodeFormat")})
            }).collect();
            json!({"quality":group["quality"].as_str().filter(|quality|matches!(*quality,"480p"|"720p"|"1080p"|"2160p")),
                "movie_count":group["movieList"].as_array().map_or(0,Vec::len),"movies":movies})
        }).collect();
        json!({"groups":groups})
    }).collect();
    record(json!({"event":"vod_options_shape","episode_count":play["episodeList"].as_array().map_or(0,Vec::len),"episodes":episodes}));
}

pub fn record_response(response: &reqwest::blocking::Response) {
    record(json!({"event":"slb_response", "host":response.url().host_str(),
        "status":response.status().as_u16(), "http_version":format!("{:?}",response.version()),
        "remote_address":response.remote_addr().map(|address|address.ip().to_string()),
        "content_length":response.content_length()}));
}
