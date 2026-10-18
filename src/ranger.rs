//! Offline reconstruction of Ranger 4.9.5 SLB V9/V11 from the supplied APK.
//! This module plans requests and decodes responses; it performs no network I/O.
//! Provenance, instruction addresses and limitations: analysis/playback/slb-protocol.md.
use aes::Aes128;
use base64::{alphabet::Alphabet, engine::{general_purpose::{GeneralPurpose, GeneralPurposeConfig}, DecodePaddingMode}, Engine};
use cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_WIRE_RESPONSE: usize = 65_536;
const MAX_PADDED_REQUEST: usize = 65_536;
const MURMUR_SEED: u32 = 0x5f325ab6;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SlbVersion { LiveV9, VodV11 }

impl SlbVersion {
    fn path(self) -> &'static str {
        match self { Self::LiveV9 => "/slb/v9/live", Self::VodV11 => "/slb/v11/vod" }
    }
}

#[derive(Deserialize)]
struct WireConstants {
    iv_prefix: String,
    v9_salt: String,
    v9_request_alphabet: String,
    v9_response_alphabet: String,
    v11_salt: String,
    v11_request_alphabet: String,
    v11_response_alphabet: String,
    content_auth_sign_salt_hex: String,
}

/// Intentionally does not implement Debug or Serialize: it contains a wire key.
pub struct SlbCodec {
    version: SlbVersion,
    key: [u8; 16],
    request_iv: [u8; 16],
    response_iv: [u8; 16],
    request_base64: GeneralPurpose,
    response_base64: GeneralPurpose,
}

impl SlbCodec {
    pub fn bundled(version: SlbVersion) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct RuntimeKey { request_key: String }
        let config: RuntimeKey = serde_json::from_str(include_str!("../analysis/protocol.constants.local.json"))
            .map_err(|_| "La configuración de reproducción está dañada.".to_string())?;
        Self::with_communication_key(version, &config.request_key)
    }

    fn with_communication_key(version: SlbVersion, communication_key: &str) -> Result<Self, String> {
        let constants: WireConstants = serde_json::from_str(include_str!("../analysis/playback/ranger.constants.local.json"))
            .map_err(|_| "Faltan las constantes de reproducción del APK.".to_string())?;
        let (salt, request_alphabet, response_alphabet) = match version {
            SlbVersion::LiveV9 => (&constants.v9_salt, &constants.v9_request_alphabet, &constants.v9_response_alphabet),
            SlbVersion::VodV11 => (&constants.v11_salt, &constants.v11_request_alphabet, &constants.v11_response_alphabet),
        };
        if salt.len() != 32 || communication_key.is_empty() {
            return Err("La clave de compatibilidad de reproducción no es válida.".into());
        }
        let key = ranger_digest(&[communication_key.as_bytes(), salt.as_bytes()].concat());
        let key_hex = hex::encode(key);
        let derive_iv = |direction: &str| {
            let material = format!("{}{}&resource={}:{}", constants.iv_prefix, key_hex, direction, version.path());
            murmur3_x64_128(material.as_bytes(), MURMUR_SEED)
        };
        let base64 = |alphabet: &str| -> Result<GeneralPurpose, String> {
            let alphabet = Alphabet::new(alphabet).map_err(|_| "El alfabeto de reproducción no es válido.".to_string())?;
            Ok(GeneralPurpose::new(&alphabet, GeneralPurposeConfig::new()
                .with_encode_padding(false).with_decode_padding_mode(DecodePaddingMode::Indifferent)))
        };
        Ok(Self { version, key, request_iv: derive_iv("req"), response_iv: derive_iv("resp"),
            request_base64: base64(request_alphabet)?, response_base64: base64(response_alphabet)? })
    }

    fn encode_plaintext(&self, plaintext: &[u8]) -> Result<String, String> {
        let padded = plaintext.len().checked_div(16).and_then(|n| n.checked_add(1)).and_then(|n| n.checked_mul(16));
        if padded.is_none_or(|n| n > MAX_PADDED_REQUEST) {
            return Err("La solicitud de reproducción es demasiado grande.".into());
        }
        let encrypted = cbc::Encryptor::<Aes128>::new((&self.key).into(), (&self.request_iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
        Ok(self.request_base64.encode(encrypted))
    }

    pub fn encode_vod(&self, authorizations: &[VodAuthorization]) -> Result<String, String> {
        if self.version != SlbVersion::VodV11 || authorizations.is_empty() {
            return Err("No hay una autorización VOD compatible para este contenido.".into());
        }
        // JsonCpp FastWriter emits compact objects, ordered keys, then LF.
        let mut plaintext = serde_json::to_vec(authorizations)
            .map_err(|_| "No se pudo preparar la autorización de vídeo.".to_string())?;
        plaintext.push(b'\n');
        self.encode_plaintext(&plaintext)
    }

    pub fn encode_live(&self, entry_auth: &str) -> Result<String, String> {
        if self.version != SlbVersion::LiveV9 || entry_auth.is_empty() {
            return Err("No hay autorización compatible para la emisión en directo.".into());
        }
        self.encode_plaintext(entry_auth.as_bytes())
    }

    pub fn decode_json(&self, wire: &str) -> Result<Value, String> {
        if wire.is_empty() || wire.len() > MAX_WIRE_RESPONSE {
            return Err("La respuesta de reproducción tiene un tamaño no válido.".into());
        }
        let ciphertext = self.response_base64.decode(wire.as_bytes())
            .map_err(|_| "El servidor devolvió una respuesta de reproducción no reconocida.".to_string())?;
        if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
            return Err("La respuesta de reproducción cifrada está incompleta.".into());
        }
        let plaintext = cbc::Decryptor::<Aes128>::new((&self.key).into(), (&self.response_iv).into())
            .decrypt_padded_vec_mut::<Pkcs7>(&ciphertext)
            .map_err(|_| "No se pudo descifrar la respuesta de reproducción.".to_string())?;
        serde_json::from_slice(&plaintext).map_err(|_| "La respuesta descifrada no contiene información de reproducción válida.".into())
    }
}

/// Backend-only media input. No account token or password belongs here.
pub struct RangerMedia {
    pub media_code: String,
    pub license: String,
}

/// Wire key ordering matches JsonCpp's object ordering. Never log this value.
#[derive(Clone, Serialize)]
pub struct VodAuthorization {
    pub auth: String,
    pub license: String,
    pub media_code: String,
}

/// The URL contains encrypted authorization. Keep it outside UI and logs.
pub struct SlbRequest {
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub version: SlbVersion,
    pub entry_name: String,
    pub entry_index: usize,
    pub endpoint_is_spare: bool,
    pub authorizations: Vec<VodAuthorization>,
    pub live_entry_auth: Option<String>,
    codec: SlbCodec,
}

impl SlbRequest {
    pub fn method(&self) -> &'static str { "GET" }
    pub fn decode(&self, wire: &str) -> Result<Vec<ResolvedMedia>, String> {
        let json = self.codec.decode_json(wire)?;
        parse_response(self.version, &json)
    }
}

/// Raw resolver output. A media_url may be a directory/template or custom
/// transport; this struct deliberately does not claim it is a playable URL.
pub struct ResolvedMedia {
    pub media_code: String,
    pub status_code: i64,
    pub metadata: Option<Value>,
    pub servers: Vec<SlbServer>,
    pub cast_servers: Vec<SlbServer>,
}

#[derive(Deserialize)]
pub struct SlbServer {
    #[serde(default)] pub tag: String,
    #[serde(default)] pub server_code: String,
    #[serde(default)] pub media_url: String,
    #[serde(default)] pub idx_url: String,
    #[serde(default)] pub snapshot_url: String,
    #[serde(default)] pub torrent_url: String,
}

/// A verified VOD SLB request. Credentials remain in the backend/relay.
/// This is not a live-channel constructor and performs no network I/O.
pub struct MediaRequest {
    pub url: Url,
    pub headers: Vec<(String, String)>,
}

/// Reconstructs the VOD SLB branch at RVA 0x4b0da8..0x4b137c. Native code
/// signs the server's request target, then restores the server's origin.
/// `instance` is a local player slot (normally 0), not an account identifier.
/// `start_moment_ms` must come from the caller's synchronized playback clock.
pub fn prepare_vod_media(server: &SlbServer, instance: i32, start_moment_ms: i64) -> Result<MediaRequest, String> {
    let mut url = Url::parse(&server.media_url)
        .map_err(|_| "El servidor no proporcionó una dirección de vídeo válida.".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("El servidor proporcionó un formato de vídeo aún no compatible.".into());
    }
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let signed = sign_content_auth(&target, instance, start_moment_ms)?;
    // Adding the signature changes only the query. Never reinterpret a local
    // /vod/..._media path as a remote CDN path or substitute a license for a URL.
    if signed != target {
        let (path, query) = signed.split_once('?')
            .ok_or_else(|| "La autorización de vídeo no tiene una consulta válida.".to_string())?;
        if path != url.path() { return Err("La autorización alteró la ruta de vídeo.".into()); }
        url.set_query(Some(query));
    }
    let mut headers = player_headers();
    headers.push(("Pragma".into(), "akamai-x-cache-on".into()));
    Ok(MediaRequest { url, headers })
}

/// Static reconstruction of 0x6b9590. The native extractor has an empty end
/// marker, so the signed token suffix includes every byte after `token=`.
pub fn sign_content_auth(auth: &str, instance: i32, start_moment_ms: i64) -> Result<String, String> {
    if auth.len() > 65_536 || auth.bytes().any(|b| b < 32 || b == 127) {
        return Err("La autorización de vídeo tiene un formato no válido.".into());
    }
    let Some((_, token_suffix)) = auth.split_once("token=") else { return Ok(auth.to_string()); };
    if token_suffix.is_empty() { return Ok(auth.to_string()); }
    if instance < 0 || start_moment_ms <= 0 {
        return Err("Falta el reloj o la instancia de reproducción.".into());
    }
    let constants: WireConstants = serde_json::from_str(include_str!("../analysis/playback/ranger.constants.local.json"))
        .map_err(|_| "Faltan las constantes de reproducción del APK.".to_string())?;
    let salt = hex::decode(constants.content_auth_sign_salt_hex)
        .map_err(|_| "La configuración de firma de vídeo no es válida.".to_string())?;
    if salt.len() != 21 { return Err("La configuración de firma de vídeo está incompleta.".into()); }
    let parameters = format!("&sign2_method=sign_o3&instance={instance}&start_moment={start_moment_ms}");
    let signature = hex::encode(ranger_digest(&[
        b"token=".as_slice(), token_suffix.as_bytes(), parameters.as_bytes(), salt.as_slice(),
    ].concat()));
    Ok(format!("{auth}{parameters}&sign2={signature}"))
}

pub fn parse_response(version: SlbVersion, json: &Value) -> Result<Vec<ResolvedMedia>, String> {
    let parse_servers = |value: Option<&Value>| -> Result<Vec<SlbServer>, String> {
        let values = value.and_then(Value::as_array)
            .ok_or_else(|| "La respuesta no contiene la lista de servidores esperada.".to_string())?;
        values.iter().map(|value| serde_json::from_value(value.clone())
            .map_err(|_| "La respuesta contiene un servidor no válido.".to_string())).collect()
    };
    if version == SlbVersion::LiveV9 {
        return Ok(vec![ResolvedMedia { media_code: String::new(), status_code: 200, metadata: None,
            servers: parse_servers(json.get("servers"))?, cast_servers: parse_servers(json.get("cast_servers"))? }]);
    }
    let infos = json.get("play_infos").and_then(Value::as_array)
        .ok_or_else(|| "La respuesta no contiene las reproducciones solicitadas.".to_string())?;
    infos.iter().map(|info| {
        let status_code = info.get("status_code").and_then(Value::as_i64).unwrap_or(0);
        let media_code = info.get("media_code").and_then(Value::as_str).unwrap_or_default().to_string();
        let metadata = info.get("metadata").and_then(Value::as_str).filter(|v| !v.is_empty())
            .and_then(|v| serde_json::from_str(v).ok());
        Ok(ResolvedMedia { media_code, status_code, metadata, servers: parse_servers(info.get("servers"))?,
            cast_servers: parse_servers(info.get("cast_servers"))? })
    }).collect()
}

/// Plans each configured Entry's main/spare requests; does not send them.
/// Native scheduling evaluates all eligible Entries, not only the first plan.
/// Pass only the selected media/language candidates, as the Java player does.
pub fn prepare_vod_requests(slb: &Value, media: &[RangerMedia]) -> Result<Vec<SlbRequest>, String> {
    let mut plans = Vec::new();
    for (entry_index, entry) in configured_vod_entries(slb)?.into_iter().enumerate() {
        let auths = entry_auths(entry);
        let authorizations: Vec<VodAuthorization> = media.iter().filter_map(|media| {
            if media.media_code.is_empty() || media.license.is_empty() { return None; }
            let app_id = auth_parameter(&media.license, "app_id");
            let tag = auth_parameter(&media.license, "tag");
            // Native search is literal, stopping at '&'; no URL-decoding step.
            let auth = auths.iter().find(|auth| auth_parameter(auth, "app_id") == app_id && auth_parameter(auth, "tag") == tag)?;
            Some(VodAuthorization { auth: (*auth).to_string(), license: media.license.clone(), media_code: media.media_code.clone() })
        }).collect();
        if authorizations.is_empty() { continue; }
        for (endpoint, endpoint_is_spare) in entry_endpoints(entry) {
            let codec = SlbCodec::bundled(SlbVersion::VodV11)?;
            let encoded = codec.encode_vod(&authorizations)?;
            if let Ok(url) = slb_url(endpoint, SlbVersion::VodV11, &encoded) {
                plans.push(SlbRequest { url, headers: default_headers(), version: SlbVersion::VodV11,
                    entry_name: string_field(entry, "cdn_id_mark").to_string(), authorizations: authorizations.clone(),
                    entry_index, endpoint_is_spare,
                    live_entry_auth: None, codec });
            }
        }
    }
    if plans.is_empty() { return Err("El servidor no proporcionó una autorización SLB compatible con este vídeo.".into()); }
    Ok(plans)
}

pub fn prepare_live_requests(slb: &Value) -> Result<Vec<SlbRequest>, String> {
    let mut plans = Vec::new();
    for (entry_index, entry) in sorted_entries(slb)?.into_iter().enumerate() {
        if string_field(entry, "cdn_type") != "1" || string_field(entry, "tag") != "live" { continue; }
        for (endpoint, endpoint_is_spare) in entry_endpoints(entry) {
            for auth in entry_auths(entry) {
                let codec = SlbCodec::bundled(SlbVersion::LiveV9)?;
                let encoded = codec.encode_live(auth)?;
                if let Ok(url) = slb_url(endpoint, SlbVersion::LiveV9, &encoded) {
                    plans.push(SlbRequest { url, headers: default_headers(), version: SlbVersion::LiveV9,
                        entry_name: string_field(entry, "cdn_id_mark").to_string(), authorizations: Vec::new(),
                        entry_index, endpoint_is_spare,
                        live_entry_auth: Some(auth.to_string()), codec });
                }
            }
        }
    }
    if plans.is_empty() { return Err("El servidor no proporcionó una autorización SLB para emisiones en directo.".into()); }
    Ok(plans)
}

fn default_headers() -> Vec<(String, String)> {
    let mut headers = player_headers();
    headers.push(("Content-Type".into(), "application/octet-stream".into()));
    headers
}

// Java p6.a.b supplies this compatibility application/version to SetEnv;
// the captured successful Android SLB and media requests confirm these values.
// Keep them on Ranger requests, separate from the portal API's desktop UA.
fn player_headers() -> Vec<(String, String)> {
    vec![
        ("User-Agent".into(), "Ranger/4.9.5-ebebe425".into()),
        ("App".into(), "com.msandroid.mobile".into()),
        ("App-Version".into(), "60291".into()),
    ]
}

fn string_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

fn sorted_entries(slb: &Value) -> Result<Vec<&Value>, String> {
    let entries = slb.get("cdn_list").and_then(Value::as_array)
        .ok_or_else(|| "Aún no se ha recibido la configuración de reproducción.".to_string())?;
    let mut entries: Vec<_> = entries.iter().collect();
    entries.sort_by_key(|entry| entry.get("serial_number").and_then(Value::as_i64).unwrap_or(0));
    Ok(entries)
}

fn configured_vod_entries(slb: &Value) -> Result<Vec<&Value>, String> {
    let entries = slb.get("cdn_list").and_then(Value::as_array)
        .ok_or_else(|| "Aún no se ha recibido la configuración de reproducción.".to_string())?;
    let mut configured = std::collections::BTreeMap::new();
    for entry in entries {
        if string_field(entry, "tag") != "vod" || entry_auths(entry).is_empty() { continue; }
        // Native 0x3c00c4 indexes by literal name+buss. A later valid
        // configuration updates that Entry instead of adding another request.
        configured.insert(format!("{}vod",string_field(entry,"cdn_id_mark")), entry);
    }
    Ok(configured.into_values().filter(|entry|string_field(entry,"cdn_type")=="1").collect())
}

fn entry_auths(entry: &Value) -> Vec<&str> {
    entry.get("url_list").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|item| item.get("url").and_then(Value::as_str)).filter(|s| !s.is_empty()).collect()
}

fn entry_endpoints(entry: &Value) -> Vec<(&str,bool)> {
    let mut out = Vec::new();
    for field in ["main_addr", "spared_addr"] {
        let address = string_field(entry, field);
        if !address.is_empty() && !out.iter().any(|(existing,_)|*existing==address) { out.push((address,field=="spared_addr")); }
    }
    out
}

fn auth_parameter<'a>(input: &'a str, field: &str) -> &'a str {
    let marker = format!("{field}=");
    // Original ARM64 extractor 0x5ca948 keeps the last literal occurrence,
    // including markers inside another value. It does not URL-decode first.
    input.rfind(&marker).map(|at| input[at + marker.len()..].split('&').next().unwrap_or_default()).unwrap_or_default()
}

#[cfg(test)]
#[test]
fn authorization_parameters_match_original_arm64_extractor() {
    let vectors: Value = serde_json::from_str(include_str!("../analysis/playback/ranger-vod-parameter-extractor.synthetic.json")).unwrap();
    for case in vectors["cases"].as_array().unwrap() {
        assert_eq!(auth_parameter(case["input"].as_str().unwrap(), case["field"].as_str().unwrap()),
            case["original_arm64"].as_str().unwrap());
    }
}

fn slb_url(endpoint: &str, version: SlbVersion, encoded: &str) -> Result<Url, String> {
    // Native fallback sets URL.scheme to the literal "https" at RVA 0x80a7f4.
    let address = if endpoint.contains("://") { endpoint.to_string() } else { format!("https://{endpoint}") };
    let mut url = Url::parse(&address).map_err(|_| "La dirección del servidor de reproducción no es válida.".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("La dirección de reproducción usa un formato no admitido.".into());
    }
    url.set_path(version.path());
    url.set_query(Some(&format!("auth={encoded}")));
    url.set_fragment(None);
    Ok(url)
}

/// Ranger's private digest at RVA 0x56ceac, NOT standard MD5. The first
/// round permutes message words and four compression constants differ.
/// Original ARM64 synthetic oracles cover this digest and complete SLB codecs.
/// Keep the portal protocol's standard MD5 separate from this compatibility code.
fn ranger_digest(input: &[u8]) -> [u8; 16] {
    const FIRST_WORDS: [usize; 16] = [10, 11, 12, 13, 14, 15, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5];
    const SHIFTS: [[u32; 4]; 4] = [[7, 12, 17, 22], [5, 9, 14, 20], [4, 11, 16, 23], [6, 10, 15, 21]];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd46f3085, 0x04881d05, 0xd9d4d039, 0xe6bd99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffecc47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2da7d2bb, 0xeb86d391,
    ];
    fn compress(state: &mut [u32; 4], block: &[u8]) {
        let words: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap()));
        let [mut a, mut b, mut c, mut d] = *state;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), FIRST_WORDS[i]),
                16..=31 => ((b & d) | (c & !d), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let next_b = b.wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(words[g])
                .rotate_left(SHIFTS[i / 16][i % 4]));
            (a, b, c, d) = (d, next_b, b, c);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d]) { *slot = slot.wrapping_add(value); }
    }
    let mut state = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut chunks = input.chunks_exact(64);
    for block in chunks.by_ref() { compress(&mut state, block); }
    let tail = chunks.remainder();
    let mut padding = [0u8; 128];
    padding[..tail.len()].copy_from_slice(tail);
    padding[tail.len()] = 0x80;
    let final_size = if tail.len() < 56 { 64 } else { 128 };
    padding[final_size - 8..final_size].copy_from_slice(&(input.len() as u64).wrapping_mul(8).to_le_bytes());
    for block in padding[..final_size].chunks_exact(64) { compress(&mut state, block); }
    let mut digest = [0u8; 16];
    for (chunk, value) in digest.chunks_exact_mut(4).zip(state) { chunk.copy_from_slice(&value.to_le_bytes()); }
    digest
}

fn murmur3_x64_128(bytes: &[u8], seed: u32) -> [u8; 16] {
    const C1: u64 = 0x87c37b91114253d5;
    const C2: u64 = 0x4cf5ad432745937f;
    let mut h1 = seed as u64;
    let mut h2 = seed as u64;
    let mut chunks = bytes.chunks_exact(16);
    for block in &mut chunks {
        let mut k1 = u64::from_le_bytes(block[..8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(block[8..].try_into().unwrap());
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2); h1 ^= k1;
        h1 = h1.rotate_left(27).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x52dce729);
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1); h2 ^= k2;
        h2 = h2.rotate_left(31).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x38495ab5);
    }
    let tail = chunks.remainder();
    let mut k1 = 0u64; let mut k2 = 0u64;
    for (n, byte) in tail.iter().enumerate() {
        if n < 8 { k1 |= (*byte as u64) << (8 * n); } else { k2 |= (*byte as u64) << (8 * (n - 8)); }
    }
    if tail.len() > 8 { h2 ^= k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1); }
    if !tail.is_empty() { h1 ^= k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2); }
    h1 ^= bytes.len() as u64; h2 ^= bytes.len() as u64;
    h1 = h1.wrapping_add(h2); h2 = h2.wrapping_add(h1);
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33; k = k.wrapping_mul(0xff51afd7ed558ccd);
        k ^= k >> 33; k = k.wrapping_mul(0xc4ceb9fe1a85ec53); k ^ (k >> 33)
    }
    h1 = fmix(h1); h2 = fmix(h2);
    h1 = h1.wrapping_add(h2); h2 = h2.wrapping_add(h1);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h1.to_le_bytes()); out[8..].copy_from_slice(&h2.to_le_bytes()); out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Value { serde_json::from_str(include_str!("../analysis/playback/ranger-vectors.synthetic.json")).unwrap() }
    fn native_fixtures() -> Value { serde_json::from_str(include_str!("../analysis/playback/ranger-vectors.native.synthetic.json")).unwrap() }
    fn codec(vector: &Value) -> SlbCodec {
        let version = if vector["version"] == 9 { SlbVersion::LiveV9 } else { SlbVersion::VodV11 };
        SlbCodec::with_communication_key(version, vector["communication_key"].as_str().unwrap()).unwrap()
    }
    #[test]
    fn original_arm64_wire_vectors() {
        for vector in native_fixtures()["vectors"].as_array().unwrap() {
            let codec = codec(vector);
            assert_eq!(hex::encode(codec.key), vector["key_hex"].as_str().unwrap());
            if vector["direction"] == "request" {
                assert_eq!(hex::encode(codec.request_iv), vector["iv_hex"].as_str().unwrap());
                assert_eq!(codec.encode_plaintext(vector["plaintext"].as_str().unwrap().as_bytes()).unwrap(), vector["encoded"].as_str().unwrap());
            } else {
                assert_eq!(hex::encode(codec.response_iv), vector["iv_hex"].as_str().unwrap());
                let decoded = codec.decode_json(vector["encoded"].as_str().unwrap()).unwrap();
                assert_eq!(decoded, serde_json::from_str::<Value>(vector["plaintext"].as_str().unwrap()).unwrap());
                let media = parse_response(codec.version, &decoded).unwrap();
                assert_eq!(media.len(), 1);
                assert_eq!(media[0].status_code, 200);
                assert_eq!(media[0].servers.len(), 1);
            }
        }
        let fixtures = native_fixtures();
        for i in [0, 5] {
            let vector = &fixtures["vectors"][i];
            let authorizations: Vec<VodAuthorization> = serde_json::from_str::<Vec<Value>>(vector["plaintext"].as_str().unwrap()).unwrap()
                .into_iter().map(|v| VodAuthorization { auth: v["auth"].as_str().unwrap().into(), license: v["license"].as_str().unwrap().into(), media_code: v["media_code"].as_str().unwrap().into() }).collect();
            assert_eq!(codec(vector).encode_vod(&authorizations).unwrap(), vector["encoded"].as_str().unwrap());
        }
        let live = &fixtures["vectors"][2];
        assert_eq!(codec(live).encode_live(live["plaintext"].as_str().unwrap()).unwrap(), live["encoded"].as_str().unwrap());
    }
    #[test]
    fn ranger_digest_matches_original_arm64() {
        for vector in native_fixtures()["native_md5"].as_array().unwrap() {
            let input = hex::decode(vector["message_hex"].as_str().unwrap()).unwrap();
            assert_eq!(hex::encode(ranger_digest(&input)), vector["digest_hex"].as_str().unwrap());
        }
    }
    #[test]
    fn murmur_reference_vectors() {
        for vector in fixtures()["hashes"].as_array().unwrap() {
            assert_eq!(hex::encode(murmur3_x64_128(vector["text"].as_str().unwrap().as_bytes(), 0)), vector["hash_hex"].as_str().unwrap());
        }
    }
    #[test]
    fn malformed_responses_and_oversized_requests_fail() {
        let codec = SlbCodec::with_communication_key(SlbVersion::VodV11, "synthetic").unwrap();
        for text in ["", "%%%%", "AA", "AAAA="] { assert!(codec.decode_json(text).is_err()); }
        assert!(codec.decode_json(&"A".repeat(MAX_WIRE_RESPONSE + 1)).is_err());
        assert!(codec.encode_plaintext(&vec![0; MAX_PADDED_REQUEST]).is_err());
        let ciphertext = vec![0u8; 16];
        assert!(codec.decode_json(&codec.response_base64.encode(ciphertext)).is_err());
    }
    #[test]
    fn request_is_get_with_main_spare_and_matching_authorization() {
        let mut slb = serde_json::json!({"cdn_list":[{"cdn_type":"1","tag":"vod","serial_number":1,
            "main_addr":"https://resolver.example.invalid/unused?discard=1", "spared_addr":"backup.example.invalid:8080",
            "url_list":[{"url":"https://auth.example.invalid/?app_id=demo&tag=wrong"},{"url":"https://auth.example.invalid/?app_id=demo&tag=sample"}]}]});
        for tag in ["record", "live", ""] {
            let mut unrelated = slb["cdn_list"][0].clone();
            unrelated["tag"] = tag.into();
            slb["cdn_list"].as_array_mut().unwrap().push(unrelated);
        }
        let media = [RangerMedia { media_code:"fake-media".into(), license:"app_id=demo&tag=sample&ticket=fake".into() }];
        let plans = prepare_vod_requests(&slb, &media).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].method(), "GET");
        assert_eq!(plans[1].url.scheme(), "https");
        assert_eq!(plans[1].url.port(), Some(8080));
        assert_eq!(plans[0].url.path(), "/slb/v11/vod");
        assert!(plans[0].url.query().unwrap().starts_with("auth="));
        assert!(!plans[0].url.query().unwrap().contains("discard"));
        assert_eq!(plans[0].authorizations[0].auth, "https://auth.example.invalid/?app_id=demo&tag=sample");
        let unmatched = [RangerMedia { media_code:"fake-media".into(), license:"app_id=other&tag=sample".into() }];
        assert!(prepare_vod_requests(&slb, &unmatched).is_err());
    }
    #[test]
    fn bare_resolver_uses_native_https_default() {
        let url = slb_url("resolver.example.invalid", SlbVersion::VodV11, "fake-wire").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.port_or_known_default(), Some(443));
        let explicit = slb_url("http://resolver.example.invalid:8080", SlbVersion::VodV11, "fake-wire").unwrap();
        assert_eq!(explicit.scheme(), "http");
        assert_eq!(explicit.port(), Some(8080));
    }
    #[test]
    fn repeated_vod_entry_uses_last_valid_configuration_once() {
        let entry=|name:&str,host:&str,auths:Value|serde_json::json!({"cdn_type":"1","tag":"vod","cdn_id_mark":name,
            "main_addr":host,"url_list":auths});
        let auths=serde_json::json!([{"url":"app_id=demo&tag=sample"}]);
        let slb=serde_json::json!({"cdn_list":[
            entry("same","old.example.invalid",auths.clone()),
            entry("other","other.example.invalid",auths.clone()),
            entry("same","new.example.invalid",auths),
            entry("same","invalid.example.invalid",serde_json::json!([]))]});
        let media=[RangerMedia{media_code:"synthetic".into(),license:"app_id=demo&tag=sample".into()}];
        let plans=prepare_vod_requests(&slb,&media).unwrap();
        assert_eq!(plans.len(),2);
        assert_eq!(plans.iter().find(|plan|plan.entry_name=="same").unwrap().url.host_str(),Some("new.example.invalid"));
        assert!(plans.iter().all(|plan|!plan.endpoint_is_spare));
        assert_ne!(plans[0].entry_index,plans[1].entry_index);
    }
    #[test]
    fn replacing_an_entry_does_not_restore_an_obsolete_transport_or_relabel_spare() {
        let mut entry=serde_json::json!({"cdn_type":"1","tag":"vod","cdn_id_mark":"same",
            "main_addr":"main.example.invalid","spared_addr":"spare.example.invalid",
            "url_list":[{"url":"app_id=demo&tag=sample"}]});
        let media=[RangerMedia{media_code:"synthetic".into(),license:"app_id=demo&tag=sample".into()}];
        let original=entry.clone();
        entry["cdn_type"]="4".into();
        assert!(prepare_vod_requests(&serde_json::json!({"cdn_list":[original.clone(),entry]}),&media).is_err());
        let mut spare_only=original;
        spare_only["main_addr"]="".into();
        let plans=prepare_vod_requests(&serde_json::json!({"cdn_list":[spare_only]}),&media).unwrap();
        assert_eq!(plans.len(),1);
        assert!(plans[0].endpoint_is_spare);
    }
    #[test]
    fn content_auth_matches_original_arm64_digest() {
        for vector in native_fixtures()["content_auth"].as_array().unwrap() {
            assert_eq!(sign_content_auth(vector["auth"].as_str().unwrap(), vector["instance"].as_i64().unwrap() as i32,
                vector["start_moment_ms"].as_i64().unwrap()).unwrap(), vector["signed"].as_str().unwrap());
        }
        assert!(sign_content_auth("/asset.mp4?token=fake", -1, 1_800_000_000_123).is_err());
        assert!(sign_content_auth("/asset.mp4?token=fake", 0, 0).is_err());
        assert!(sign_content_auth("/asset.mp4?token=fake\r\n", 0, 1_800_000_000_123).is_err());
    }
    #[test]
    fn vod_media_preserves_origin_and_never_invents_remote_paths() {
        let mut server: SlbServer = serde_json::from_value(serde_json::json!({"media_url":"https://media.example.invalid:8443/assets/movie.mp4?token=synthetic"})).unwrap();
        let request = prepare_vod_media(&server, 0, 1_800_000_000_123).unwrap();
        assert_eq!(request.url.host_str(), Some("media.example.invalid"));
        assert_eq!(request.url.port(), Some(8443));
        assert_eq!(request.url.path(), "/assets/movie.mp4");
        assert!(request.url.query().unwrap().contains("&sign2_method=sign_o3&instance=0&start_moment=1800000000123&sign2="));
        assert!(request.headers.contains(&("Pragma".into(), "akamai-x-cache-on".into())));
        assert!(request.headers.contains(&("User-Agent".into(), "Ranger/4.9.5-ebebe425".into())));
        assert!(request.headers.contains(&("App".into(), "com.msandroid.mobile".into())));
        assert!(request.headers.contains(&("App-Version".into(), "60291".into())));
        server.media_url = "https://media.example.invalid/assets/movie.mp4".into();
        assert_eq!(prepare_vod_media(&server, 0, 1_800_000_000_123).unwrap().url.as_str(), server.media_url);
        for invalid in ["", "license=synthetic", "ranger://media/asset", "https://user:fake@media.example.invalid/a", "https://media.example.invalid/a#fragment"] {
            server.media_url = invalid.into();
            assert!(prepare_vod_media(&server, 0, 1_800_000_000_123).is_err());
        }
    }
    #[test]
    fn live_uses_entry_auth_and_vod_requires_play_infos() {
        let slb = serde_json::json!({"cdn_list":[{"cdn_type":"1","tag":"live","main_addr":"https://resolver.example.invalid",
            "url_list":[{"url":"app_id=fake&tag=one"},{"url":"app_id=fake&tag=two"}]}]});
        let plans = prepare_live_requests(&slb).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].url.path(), "/slb/v9/live");
        assert_eq!(plans[0].live_entry_auth.as_deref(), Some("app_id=fake&tag=one"));
        assert!(parse_response(SlbVersion::VodV11, &serde_json::json!({"vod":[]})).is_err());
        assert!(parse_response(SlbVersion::VodV11, &serde_json::json!({"play_infos":[{"status_code":200,"servers":[]}]})).is_err());
    }
}
