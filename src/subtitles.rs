//! Portal-authorized subtitle tracks. Remote URLs stay in the backend; only
//! bounded, sanitized WebVTT is sent to the desktop after a track is selected.
use std::{io::Read, time::{Duration, Instant}};
use reqwest::Url;
use md5::{Digest, Md5};
use serde::Serialize;
use serde_json::Value;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_CUES: usize = 20000;

#[derive(Serialize)]
pub struct Track { pub id: String, pub label: String, pub language: String }
pub struct SubtitleSource { pub track: Track, pub url: Url }

pub fn sources(play: &Value, content_id: &str, episode_number: Option<i64>, authority: Option<&str>) -> Result<Vec<SubtitleSource>, String> {
    let episode = crate::models::vod_episode(play, content_id, episode_number)?;
    let mut result: Vec<SubtitleSource> = Vec::new();
    for group in episode.get("subtitleList").and_then(Value::as_array).into_iter().flatten().take(64) {
        let language = group.get("language").and_then(Value::as_str).unwrap_or("");
        if language.len() > 64 || language.chars().any(char::is_control) { continue; }
        let normalized = language.to_ascii_lowercase().replace('_', "-");
        let label = if crate::models::valid_audio_language(&normalized) {
            let base = normalized.split('-').next().unwrap_or(&normalized);
            let label = crate::models::audio_label(base);
            if normalized.contains('-') { format!("{label} ({language})") } else { label }
        } else if language.is_empty() { "Subtítulo".into() } else { language.to_string() };
        for file in group.get("file").and_then(Value::as_array).into_iter().flatten().take(64) {
            if file.get("fileType").and_then(Value::as_str) != Some("srt") { continue; }
            let Some(original) = file.get("url").and_then(Value::as_str).filter(|url| url.len() <= 8192) else { continue; };
            let Some(valid) = crate::models::http_url(original) else { continue; };
            let mut url = Url::parse(&valid).map_err(|_| "La dirección del subtítulo no es válida.")?;
            if url.fragment().is_some() { continue; }
            if let Some(authority) = authority.filter(|value| !value.is_empty()) {
                // z2.e.a replaces authority only, keeping scheme/path/query.
                let host = Url::parse(&format!("{}://{authority}/", url.scheme())).map_err(|_| "El servidor de subtítulos no es válido.")?;
                if !host.username().is_empty() || host.password().is_some() || host.path() != "/" || host.query().is_some() || host.fragment().is_some() {
                    return Err("El servidor de subtítulos no es válido.".into());
                }
                url.set_host(host.host_str()).map_err(|_| "El servidor de subtítulos no es válido.")?;
                url.set_port(host.port()).map_err(|_| "El servidor de subtítulos no es válido.")?;
            }
            let mut digest = Md5::new();
            digest.update(language.as_bytes()); digest.update([0u8]); digest.update(original.as_bytes());
            let id = hex::encode(digest.finalize());
            if result.iter().any(|source| source.track.id == id) { continue; }
            if result.len() >= 64 { return Err("El servicio devolvió demasiadas pistas de subtítulos.".into()); }
            result.push(SubtitleSource { track: Track { id, label: label.clone(),
                language: if crate::models::valid_audio_language(&normalized) { normalized.clone() } else { String::new() } }, url });
        }
    }
    Ok(result)
}

pub fn fetch_vtt(mut url: Url) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(25);
    for hop in 0..=4 {
        let (builder, _) = crate::protocol::media_client_builder_until(&url, Some(deadline))?;
        let response = builder.build().map_err(|_| "No se pudo preparar el subtítulo.")?
            .get(url.clone())
            .timeout(crate::protocol::remaining_timeout(Some(deadline), Duration::from_secs(20))?)
            .send().map_err(|_| "No se pudo descargar el subtítulo. Inténtalo de nuevo.")?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if hop == 4 { return Err("El subtítulo devolvió demasiadas redirecciones.".into()); }
            let location = response.headers().get(reqwest::header::LOCATION).and_then(|value| value.to_str().ok())
                .ok_or("El subtítulo devolvió una redirección inválida.")?;
            url = url.join(location).map_err(|_| "La dirección del subtítulo no es válida.")?;
            // Every next hop goes through the same public-address checks.
            // Portal credentials and video authorization headers are never sent.
            continue;
        }
        if response.status().as_u16() != 200 { return Err("Este subtítulo no está disponible en el servicio.".into()); }
        if response.content_length().is_some_and(|length| length > MAX_BYTES as u64) {
            return Err("El archivo de subtítulos es demasiado grande.".into());
        }
        let mut bytes = Vec::new();
        response.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)
            .map_err(|_| "La descarga del subtítulo quedó incompleta.")?;
        if bytes.len() > MAX_BYTES { return Err("El archivo de subtítulos es demasiado grande.".into()); }
        return srt_to_vtt(&bytes);
    }
    unreachable!()
}

fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if bytes.len() % 2 != 0 { return Err("El subtítulo tiene una codificación incompleta.".into()); }
        let little_endian = bytes[0] == 0xff;
        let units: Vec<_> = bytes[2..].chunks_exact(2).map(|pair| if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else { u16::from_be_bytes([pair[0], pair[1]]) }).collect();
        return String::from_utf16(&units).map_err(|_| "El subtítulo tiene una codificación inválida.".into());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if let Ok(value) = std::str::from_utf8(bytes) { return Ok(value.to_string()); }
    // Common legacy SRT encoding; preserve Spanish/Portuguese accents.
    const CP1252: [char; 32] = ['€','\u{fffd}','‚','ƒ','„','…','†','‡','ˆ','‰','Š','‹','Œ','\u{fffd}','Ž','\u{fffd}',
        '\u{fffd}','‘','’','“','”','•','–','—','˜','™','š','›','œ','\u{fffd}','ž','Ÿ'];
    Ok(bytes.iter().map(|&byte| if (0x80..=0x9f).contains(&byte) { CP1252[(byte - 0x80) as usize] } else { byte as char }).collect())
}

fn timestamp(value: &str) -> Option<(u64, String)> {
    let mut parts = value.trim().split([':', ',', '.']);
    let hours = parts.next()?.parse::<u64>().ok()?;
    let minutes = parts.next()?.parse::<u64>().ok()?;
    let seconds = parts.next()?.parse::<u64>().ok()?;
    let fraction = parts.next()?;
    if parts.next().is_some() || hours > 999 || minutes >= 60 || seconds >= 60 || fraction.len() != 3 { return None; }
    let millis = fraction.parse::<u64>().ok()?;
    if millis >= 1000 { return None; }
    Some((((hours * 60 + minutes) * 60 + seconds) * 1000 + millis,
        format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")))
}

fn cue_text(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.starts_with("{\\") {
            if let Some(end) = remaining.as_bytes().iter().take(513).position(|byte| *byte == b'}') {
                remaining = &remaining[end + 1..];
                continue;
            }
        }
        if remaining.starts_with('<') {
            if let Some(end) = remaining.as_bytes().iter().take(257).position(|byte| *byte == b'>') {
                let tag = &remaining[1..end];
                if ["b", "/b", "i", "/i", "u", "/u"].contains(&tag.to_ascii_lowercase().as_str()) {
                    result.push('<'); result.push_str(&tag.to_ascii_lowercase()); result.push('>');
                } else if tag.to_ascii_lowercase().starts_with("font ") || tag.eq_ignore_ascii_case("/font") {
                    // Styling is controlled by the user's subtitle preferences.
                } else {
                    result.push_str("&lt;"); result.push_str(tag); result.push_str("&gt;");
                }
                remaining = &remaining[end + 1..];
                continue;
            }
        }
        let character = remaining.chars().next().unwrap();
        match character {
            '<' => result.push_str("&lt;"), '>' => result.push_str("&gt;"),
            '\0' => {}, c if c.is_control() && c != '\n' && c != '\t' => {},
            c => result.push(c),
        }
        remaining = &remaining[character.len_utf8()..];
    }
    result
}

pub fn srt_to_vtt(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_BYTES { return Err("El archivo de subtítulos es demasiado grande.".into()); }
    let text = decode_text(bytes)?.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::from("WEBVTT\n\n");
    let mut count = 0;
    let lines: Vec<_> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let Some((start, end)) = lines[index].split_once("-->") else { index += 1; continue; };
        let end = end.split_whitespace().next().unwrap_or("");
        let (Some((start_ms, start)), Some((end_ms, end))) = (timestamp(start), timestamp(end)) else {
            index += 1; continue;
        };
        index += 1;
        let begin = index;
        while index < lines.len() && !lines[index].trim().is_empty() { index += 1; }
        if end_ms <= start_ms || begin == index { continue; }
        let sanitized = cue_text(&lines[begin..index].join("\n"));
        let sanitized = sanitized.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>().join("\n");
        if sanitized.trim().is_empty() { continue; }
        count += 1;
        if count > MAX_CUES { return Err("El subtítulo contiene demasiados fragmentos.".into()); }
        output.push_str(&format!("{start} --> {end}\n{sanitized}\n\n"));
        if output.len() > MAX_BYTES * 4 { return Err("El subtítulo convertido es demasiado grande.".into()); }
    }
    if count == 0 { return Err("El servicio no devolvió un subtítulo SRT válido.".into()); }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selected_episode_srt_tracks_are_exposed_without_remote_urls() {
        let data = serde_json::json!({"episodeList":[
            {"programContentId":"other", "episodeNumber":1, "subtitleList":[{"language":"en","file":[{"fileType":"srt","url":"https://example.invalid/other.srt"}]}]},
            {"programContentId":"selected", "episodeNumber":2, "subtitleList":[{"language":"es", "file":[
                {"fileType":"srt","url":"https://example.invalid/captions.srt?auth=private"},
                {"fileType":"ass","url":"https://example.invalid/wrong.ass"},
                {"fileType":"srt","url":"file:///private.srt"}]}]}]});
        let tracks = sources(&data, "selected", Some(2), Some("sub.example.invalid:8443")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track.label, "Español");
        assert_eq!(tracks[0].url.as_str(), "https://sub.example.invalid:8443/captions.srt?auth=private");
        let exposed = serde_json::to_string(&tracks[0].track).unwrap();
        assert!(!exposed.contains("private"));
        assert!(!exposed.contains("http"));
        assert!(sources(&data, "absent", Some(3), None).is_err());
        assert!(sources(&data, "selected", Some(2), Some("user:password@example.invalid")).is_err());
    }

    #[test]
    fn converts_real_srt_shapes_and_keeps_multiline_dialogue() {
        let result = srt_to_vtt("\u{feff}1\r\n00:00:01,250 --> 00:00:04,900\r\n<i>¡Hola!</i>\r\nSegunda línea.\r\n\r\n2\r\n00:00:05,000 --> 00:00:06,000 X1:0\r\n{\\an8}<font color=\"red\">Adiós</font>\r\n".as_bytes()).unwrap();
        assert_eq!(result, "WEBVTT\n\n00:00:01.250 --> 00:00:04.900\n<i>¡Hola!</i>\nSegunda línea.\n\n00:00:05.000 --> 00:00:06.000\nAdiós\n\n");
    }

    #[test]
    fn handles_utf16_and_legacy_accents_and_rejects_html_error_pages() {
        let input = "1\n00:00:01,000 --> 00:00:02,000\nEspañol, ação\n";
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(input.encode_utf16().flat_map(u16::to_le_bytes));
        assert!(srt_to_vtt(&bytes).unwrap().contains("Español, ação"));
        assert!(srt_to_vtt(b"1\n00:00:01,000 --> 00:00:02,000\nEspa\xf1ol \x93texto\x94").unwrap().contains("Español “texto”"));
        assert!(srt_to_vtt(b"<html><body>Access denied</body></html>").is_err());
        assert!(srt_to_vtt(b"1\n00:60:01,000 --> 00:60:02,000\ninvalid").is_err());
    }

    #[test]
    fn subtitles_cannot_inject_vtt_styles_or_executable_markup() {
        let input = b"1\n00:00:01,000 --> 00:00:02,000\n<script>alert(1)</script><b>okay</b>\0\n\nSTYLE\n::cue {background:url(https://example.invalid)}\n";
        let vtt = srt_to_vtt(input).unwrap();
        assert!(!vtt.contains("<script>"));
        assert!(!vtt.contains("STYLE"));
        assert!(vtt.contains("<b>okay</b>"));
    }

    #[test]
    fn stripped_style_lines_do_not_terminate_a_cue() {
        let input = b"1\n00:00:01,000 --> 00:00:02,000\n{\\an8}\n<font color=\"red\">\nHola\n</font>\n";
        assert_eq!(srt_to_vtt(input).unwrap(), "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHola\n\n");
    }
}
