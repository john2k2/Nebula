//! Wire compatibility established from ka.c/ka.d, ha.a/ha.d and u7.a/u7.b.
//! No account, device identity, user token or media license is bundled here.
use base64::{engine::general_purpose::STANDARD, Engine};
use cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use des::TdesEde3;
use md5::{Digest, Md5};
use reqwest::{blocking::Client, Url};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, io::Read, net::{IpAddr, Ipv4Addr, SocketAddr}, time::{Duration, Instant}};

const MAX_RESPONSE: u64 = 32 * 1024 * 1024;
const MAX_DNS_RESPONSE: u64 = 1024 * 1024;
pub const SESSION_REPLACED_MESSAGE: &str = "Tu sesión ya no está activa. Pulsa Reconectar para continuar.";

pub fn is_session_expired_error(message: &str) -> bool {
    message == SESSION_REPLACED_MESSAGE
}

#[cfg(test)]
struct ScriptedTransport {
    replies: std::collections::VecDeque<(String, Result<Value, String>)>,
    calls: Vec<(String, Value)>,
}

#[derive(Deserialize)]
struct Bundled {
    portal_endpoints: Vec<String>,
    portal_encrypted: bool,
    domain_key: String,
    request_key: String,
    response_key: String,
    app_id: String,
    api_version: u32,
    source_apk_certificate_digest: String,
    sn_md5_suffix: String,
    routes: HashMap<String, String>,
}

pub struct Protocol {
    client: Client,
    config: Bundled,
    endpoints: Vec<Url>,
    endpoint: usize,
    dns_valid_until: Option<Instant>,
    #[cfg(test)]
    scripted: Option<ScriptedTransport>,
}

impl Protocol {
    pub fn new() -> Result<Self, String> {
        let config: Bundled = serde_json::from_str(include_str!("../analysis/protocol.constants.local.json"))
            .map_err(|_| "La configuración de compatibilidad está dañada.".to_string())?;
        let mut endpoints = Vec::new();
        for encoded in &config.portal_endpoints {
            let endpoint = if config.portal_encrypted {
                // m7.b.a preserves the original resource when decryption fails.
                // This variant's portal resources are already plain hostnames.
                decrypt(encoded, &config.domain_key).unwrap_or_else(|_|encoded.clone())
            } else {
                encoded.clone()
            };
            let endpoint = normalize_endpoint(&endpoint)?;
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        }
        if endpoints.is_empty() {
            return Err("El APK no contiene un servidor de catálogo válido.".into());
        }
        let client = portal_client_builder()
            .build()
            .map_err(|_| "No se pudo iniciar la conexión segura de Windows.".to_string())?;
        Ok(Self { client, config, endpoints, endpoint: 0, dns_valid_until: None,
            #[cfg(test)] scripted: None })
    }

    /// Offline test transport: an exhausted script fails the test rather than
    /// falling through to DNS or HTTP. Replies use the real response parser.
    #[cfg(test)]
    pub(crate) fn scripted(replies: Vec<(&str, Result<Value, String>)>) -> Self {
        let mut protocol = Self::new().unwrap();
        protocol.scripted = Some(ScriptedTransport {
            replies: replies.into_iter().map(|(method, reply)| (method.to_string(), reply)).collect(),
            calls: Vec::new(),
        });
        protocol
    }

    #[cfg(test)]
    pub(crate) fn scripted_calls(&self) -> &[(String, Value)] {
        &self.scripted.as_ref().expect("Scripted transport required").calls
    }

    pub fn api_version(&self) -> u32 { self.config.api_version }

    pub fn sn_digest(&self, token: &str) -> String {
        let mut digest = Md5::new();
        digest.update(token.as_bytes());
        digest.update(self.config.sn_md5_suffix.as_bytes());
        hex::encode(digest.finalize())
    }

    /// Public digest of the supplied source APK, not a claim about the EXE signer.
    pub fn source_apk_certificate_digest(&self) -> &str {
        &self.config.source_apk_certificate_digest
    }

    pub fn common(&self, sn: &str) -> Map<String, Value> {
        let mut values = Map::new();
        for (name, value) in [
            ("loginType", "3".to_string()),
            ("appLanguage", "es".to_string()),
            ("apkVersion", self.config.api_version.to_string()),
            ("sysVersion", "Windows".to_string()),
            ("appId", self.config.app_id.clone()),
            ("hardwareInfo", std::env::consts::ARCH.to_string()),
            ("model", "Windows Desktop".to_string()),
            ("product", "Xuper Desktop".to_string()),
            ("cpu", std::env::consts::ARCH.to_string()),
            ("deviceToken", String::new()),
            ("reserve1", String::new()),
            ("sn", sn.to_string()),
            ("drmId", String::new()),
        ] {
            values.insert(name.to_string(), Value::String(value));
        }
        values
    }

    pub fn post_until(&mut self, method: &str, body: Value, sn: &str, deadline: Option<Instant>) -> Result<Value, String> {
        let route = self.config.routes.get(method)
            .cloned().ok_or_else(|| "Operación ausente en el contrato del APK.".to_string())?;
        // This backend exposes only enrollment, read and playback authorization.
        if !["o", "D", "e0", "y", "Z", "x0", "i", "g", "K", "K0", "H", "z0"].contains(&method) {
            return Err("La operación no está habilitada en este cliente.".into());
        }
        let mut payload = body.as_object().cloned()
            .ok_or_else(|| "La solicitud interna no es un objeto.".to_string())?;
        payload.extend(self.common(sn));
        // Gson's default serialization omits null fields.
        payload.retain(|_, value| !value.is_null());
        let operation = route.rsplit('/').next().unwrap_or("consulta");
        #[cfg(test)]
        if let Some(scripted) = self.scripted.as_mut() {
            scripted.calls.push((method.to_string(), Value::Object(payload.clone())));
            let (expected, reply) = scripted.replies.pop_front().expect("Unexpected request; offline script is exhausted");
            assert_eq!(expected, method, "Unexpected operation in offline session test");
            return self.decode_response(reply?, operation, &payload);
        }
        let plaintext = serde_json::to_string(&payload).map_err(|_| "No se pudo preparar la solicitud.".to_string())?;
        let encoded = encrypt(&plaintext, &self.config.request_key)?;
        if self.dns_valid_until.is_some_and(|expiry| expiry <= Instant::now()) {
            self.refresh_portal_dns(deadline)?;
        }
        let mut dns_attempted = self.dns_valid_until.is_some();
        let mut attempt = 0;
        let response = loop {
            let timeout=remaining_timeout(deadline, Duration::from_secs(30))?;
            let target = self.endpoints[self.endpoint].join(&route)
                .map_err(|_| "La dirección de la operación es inválida.".to_string())?;
            let result = self.client.post(target)
                .header("Content-Type", "application/json;charset=utf-8")
                .header("Cache-Control", "no-cache")
                .header("apk", &self.config.app_id)
                .header("apkVer", self.config.api_version.to_string())
                .header("spkgVer", "Windows")
                .timeout(timeout)
                .body(encoded.clone())
                .send();
            match result {
                Ok(response) => break response,
                // A connect error occurs before an HTTP request is sent. Only
                // this case can retry enrollment; a response timeout cannot.
                Err(error) if error.is_connect() && !dns_attempted => {
                    dns_attempted = true;
                    self.refresh_portal_dns(deadline)?;
                }
                Err(error) if error.is_connect() && attempt + 1 < self.endpoints.len() => {
                    attempt += 1;
                    self.endpoint = (self.endpoint + 1) % self.endpoints.len();
                }
                Err(error) => return Err(if error.is_timeout() {
                    format!("El servidor tardó demasiado en responder a {operation}.")
                } else {
                    format!("No se pudo conectar de forma segura para {operation}.")
                }),
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(format!("El servidor respondió HTTP {} al solicitar {operation}.", status.as_u16()));
        }
        let mut bytes = Vec::new();
        response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)
            .map_err(|_| format!("La respuesta de {operation} quedó incompleta."))?;
        if bytes.len() as u64 > MAX_RESPONSE {
            return Err("La respuesta del servidor excede el tamaño permitido.".into());
        }
        let result: Value = serde_json::from_slice(&bytes)
            .map_err(|_| format!("El servidor no devolvió JSON válido para {operation}."))?;
        self.decode_response(result, operation, &payload)
    }

    fn decode_response(&self, mut result: Value, operation: &str, payload: &Map<String, Value>) -> Result<Value, String> {
        let code = result.get("returnCode").and_then(scalar);
        // Observed responses: an active window's replaced token (aaa100083)
        // and getAuthInfo validating that same stale token (aaa100027).
        // Do not classify unrelated service errors by their message text.
        if matches!(code.as_deref(), Some("aaa100083" | "aaa100027")) {
            return Err(SESSION_REPLACED_MESSAGE.into());
        }
        if code.as_deref() != Some("0") {
            let code = code.unwrap_or_else(|| "sin código".to_string());
            // Avoid propagating opaque server data, tokens, or diagnostic URLs.
            let message = result.get("errorMessage").and_then(Value::as_str).unwrap_or("");
            let safe_message = safe_server_message(message, payload, &self.config);
            return Err(format!("El servicio rechazó {operation} (código {code}){safe_message}"));
        }
        if let Some(ciphertext) = result.get("data").and_then(Value::as_str) {
            if ciphertext.is_empty() {
                result["data"] = Value::Null;
            } else {
                let plain = decrypt(ciphertext, &self.config.response_key)?;
                result["data"] = serde_json::from_str(&plain)
                    .map_err(|_| "El contenido descifrado del servidor no es JSON válido.".to_string())?;
            }
        }
        Ok(result.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Android's NF variant tunnels DNS. Windows uses a bounded process-local
    /// HTTPS resolver only after the ordinary connection fails. Neither system
    /// DNS nor TLS validation is changed, and no app identity is sent to DoH.
    fn refresh_portal_dns(&mut self, deadline: Option<Instant>) -> Result<(), String> {
        let resolver = dns_client()?;
        let mut builder = portal_client_builder();
        let mut lifetime = 3600;
        let mut resolved = 0;
        let mut last_error = String::new();
        for endpoint in &self.endpoints {
            let host = endpoint.host_str().ok_or("El portal no contiene un nombre DNS válido.")?;
            let record = dns_addresses_until(&resolver, host, deadline);
            if let Ok((addresses, ttl)) = record {
                builder = builder.resolve_to_addrs(host, &addresses);
                lifetime = lifetime.min(ttl);
                resolved += 1;
            } else if let Err(error)=record {
                last_error=error;
            }
        }
        if resolved == 0 {
            return Err(format!("No se pudieron resolver los portales mediante DNS seguro. {last_error}"));
        }
        self.client = builder.build()
            .map_err(|_| "No se pudo iniciar la conexión segura de Windows.".to_string())?;
        // TTL zero is deliberately not extended. The next command resolves
        // again; no address is permanently pinned or written to disk.
        self.dns_valid_until = Some(Instant::now() + Duration::from_secs(lifetime));
        Ok(())
    }
}

fn portal_client_builder() -> reqwest::blocking::ClientBuilder {
    transport_client_builder().user_agent("Xuper-Desktop/0.1 (Windows; Rust)")
}

fn transport_client_builder() -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4)
        .redirect(reqwest::redirect::Policy::none())
}

fn dns_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build().map_err(|_| "No se pudo iniciar la resolución DNS segura.".to_string())
}

fn dns_addresses_until(resolver: &Client, host: &str, deadline: Option<Instant>) -> Result<(Vec<SocketAddr>, u64), String> {
    let mut last_error=String::new();
    for provider in ["https://dns.google/resolve","https://cloudflare-dns.com/dns-query"] {
        match dns_addresses_at_until(resolver,host,provider,deadline) {
            Ok(answer)=>return Ok(answer),
            Err(error)=>last_error=error,
        }
    }
    Err(last_error)
}

fn dns_addresses_at_until(resolver:&Client,host:&str,provider:&str,deadline:Option<Instant>)->Result<(Vec<SocketAddr>,u64),String>{
    let response = resolver.get(provider)
        .query(&[("name", host), ("type", "A")])
        .header("Accept", "application/dns-json")
        .timeout(remaining_timeout(deadline,Duration::from_secs(5))?)
        .send().map_err(|error| if error.is_timeout(){"El DNS seguro tardó demasiado en responder.".to_string()}else{"No se pudo conectar de forma segura con el DNS público.".to_string()})?;
    if !response.status().is_success() {
        return Err(format!("El DNS seguro respondió HTTP {}.",response.status().as_u16()));
    }
    let mut bytes = Vec::new();
    response.take(MAX_DNS_RESPONSE + 1).read_to_end(&mut bytes)
        .map_err(|_| "La respuesta DNS quedó incompleta.".to_string())?;
    if bytes.len() as u64 > MAX_DNS_RESPONSE {
        return Err("La respuesta DNS excede el tamaño permitido.".into());
    }
    let result: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "El DNS seguro devolvió una respuesta inválida.".to_string())?;
    parse_dns_response(&result)
}

/// A media/SLB transport builder without portal credentials or default UA.
/// Ranger plans supply the effective player headers set by the native Env.
/// The caller must
/// honor the returned DNS lifetime when caching the built client, and controls
/// streaming timeouts separately from the default 30-second API timeout.
pub fn media_client_builder(url: &Url) -> Result<(reqwest::blocking::ClientBuilder, Duration), String> {
    media_client_builder_until(url,None)
}

pub fn media_client_builder_until(url: &Url, deadline: Option<Instant>) -> Result<(reqwest::blocking::ClientBuilder, Duration), String> {
    remaining_timeout(deadline,Duration::from_secs(30))?;
    if !["http", "https"].contains(&url.scheme()) || !url.username().is_empty() || url.password().is_some() {
        return Err("La dirección de reproducción no es compatible.".into());
    }
    let host = url.host_str().ok_or("La reproducción no contiene un servidor válido.")?;
    if let Ok(address) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        let public = match address {
            IpAddr::V4(address) => public_ipv4(&address),
            IpAddr::V6(address) => !address.is_loopback() && !address.is_unspecified()
                && !address.is_multicast() && !address.is_unique_local() && !address.is_unicast_link_local()
                && address.to_ipv4_mapped().is_none()
                && address.segments()[..2] != [0x2001, 0x0db8],
        };
        if !public { return Err("El servidor de reproducción no tiene una dirección pública válida.".into()); }
        return Ok((transport_client_builder(), Duration::from_secs(300)));
    }
    let (addresses, ttl) = dns_addresses_until(&dns_client()?, host, deadline)?;
    Ok((transport_client_builder().resolve_to_addrs(host, &addresses), Duration::from_secs(ttl.min(300))))
}

pub fn remaining_timeout(deadline:Option<Instant>,maximum:Duration)->Result<Duration,String>{
    let remaining=deadline.map(|deadline|deadline.saturating_duration_since(Instant::now())).unwrap_or(maximum).min(maximum);
    if remaining.is_zero(){Err("La preparación del vídeo tardó demasiado. Inténtalo de nuevo.".into())}else{Ok(remaining)}
}

fn parse_dns_response(result: &Value) -> Result<(Vec<SocketAddr>, u64), String> {
    if result.get("Status").and_then(Value::as_u64) != Some(0) {
        return Err("El DNS seguro no pudo resolver el portal.".into());
    }
    let answers = result.get("Answer").and_then(Value::as_array)
        .ok_or("El DNS seguro no devolvió direcciones para el portal.")?;
    let mut addresses = Vec::new();
    let mut ttl = 3600;
    for answer in answers {
        if answer.get("type").and_then(Value::as_u64) == Some(5) {
            if let Some(lifetime)=answer.get("TTL").and_then(Value::as_u64){ttl=ttl.min(lifetime);}
            continue;
        }
        if answer.get("type").and_then(Value::as_u64) != Some(1) { continue; }
        let Some(address) = answer.get("data").and_then(Value::as_str)
            .and_then(|s| s.parse::<Ipv4Addr>().ok()).filter(public_ipv4) else { continue; };
        let Some(lifetime) = answer.get("TTL").and_then(Value::as_u64) else { continue; };
        let address = SocketAddr::new(IpAddr::V4(address), 0);
        if !addresses.contains(&address) { addresses.push(address); }
        ttl = ttl.min(lifetime);
    }
    if addresses.is_empty() {
        return Err("El DNS seguro no devolvió direcciones públicas válidas.".into());
    }
    Ok((addresses, ttl))
}

fn public_ipv4(address: &Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private() && !address.is_loopback() && !address.is_link_local()
        && !address.is_unspecified() && !address.is_broadcast() && !address.is_multicast()
        && octets[0] != 0 && octets[0] < 240
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 198 && (18..=19).contains(&octets[1]))
        && !address.is_documentation()
}

fn safe_server_message(message: &str, payload: &Map<String, Value>, config: &Bundled) -> String {
    if message.contains("http://") || message.contains("https://") || message.len() > 500 {
        return ".".into();
    }
    let mut result = message.to_string();
    for key in ["userToken", "snToken", "sn", "userId", "androidId"] {
        if let Some(value) = payload.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            result = result.replace(value, "[dato omitido]");
        }
    }
    for secret in [&config.request_key, &config.response_key, &config.domain_key] {
        if !secret.is_empty() { result = result.replace(secret, "[dato omitido]"); }
    }
    result.retain(|c| !c.is_control());
    if result.is_empty() { ".".into() } else { format!(": {result}") }
}

pub fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn normalize_endpoint(value: &str) -> Result<Url, String> {
    let value = value.trim();
    // la.t.a removes either scheme and selects HTTPS.
    let value = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://")).unwrap_or(value);
    let with_scheme = format!("https://{value}");
    let url = Url::parse(&with_scheme).map_err(|_| "El servidor configurado tiene una dirección inválida.".to_string())?;
    if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("El servidor configurado tiene una dirección incompatible.".into());
    }
    Ok(url)
}

fn cipher(key: &str) -> Result<TdesEde3, String> {
    let key = legacy_key_decode(key)?;
    if key.len() < 24 { return Err("La clave de compatibilidad tiene una longitud inválida.".into()); }
    TdesEde3::new_from_slice(&key[..24]).map_err(|_| "No se pudo iniciar el cifrado del protocolo.".into())
}

// s7.a/t7.c follows the old Java CharacterDecoder table semantics, including
// table value -1 for non-alphabet characters. The bundled UUID-shaped keys use
// that behavior; treating them as strict or URL-safe Base64 changes the key.
// Ciphertext itself is standard Base64 and remains strictly checked above.
fn legacy_key_decode(input: &str) -> Result<Vec<u8>, String> {
    fn digit(c:u8)->i32 {
        match c { b'A'..=b'Z'=>(c-b'A') as i32,b'a'..=b'z'=>(c-b'a'+26) as i32,b'0'..=b'9'=>(c-b'0'+52) as i32,b'+'=>62,b'/'=>63,_=>-1 }
    }
    let input:Vec<u8>=input.bytes().filter(|c|*c!=b'\r' && *c!=b'\n').collect();
    let mut result=Vec::new();
    for block in input.chunks(4) {
        if block.len()<2 { return Err("La clave de compatibilidad no tiene un formato válido.".into()); }
        let length=if block.get(2)==Some(&b'='){2}else if block.get(3)==Some(&b'='){3}else{block.len()};
        let a=digit(block[0]);let b=digit(block[1]);
        result.push((((a<<2)&0xfc)|((b>>4)&3)) as u8);
        if length>2 {
            let c=digit(block[2]);
            result.push((((b<<4)&0xf0)|((c>>2)&15)) as u8);
            if length>3 {result.push((((c<<6)&0xc0)|(digit(block[3])&63)) as u8);}
        }
    }
    Ok(result)
}

pub fn encrypt(plaintext: &str, key: &str) -> Result<String, String> {
    let cipher = cipher(key)?;
    let mut bytes = plaintext.as_bytes().to_vec();
    let padding = 8 - bytes.len() % 8;
    bytes.extend(std::iter::repeat_n(padding as u8, padding));
    for block in bytes.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(hex::encode(STANDARD.encode(bytes).as_bytes()))
}

pub fn decrypt(ciphertext: &str, key: &str) -> Result<String, String> {
    let encoded = hex::decode(ciphertext).map_err(|_| "La respuesta cifrada tiene un formato inválido.".to_string())?;
    let encoded: Vec<u8> = encoded.into_iter().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut bytes = STANDARD.decode(encoded).map_err(|_| "La respuesta cifrada contiene datos inválidos.".to_string())?;
    if bytes.is_empty() || bytes.len() % 8 != 0 {
        return Err("La respuesta cifrada está incompleta.".into());
    }
    let cipher = cipher(key)?;
    for block in bytes.chunks_exact_mut(8) { cipher.decrypt_block(GenericArray::from_mut_slice(block)); }
    let padding = *bytes.last().unwrap() as usize;
    if !(1..=8).contains(&padding) || !bytes[bytes.len()-padding..].iter().all(|b| *b as usize == padding) {
        return Err("No se pudo verificar el formato de la respuesta cifrada.".into());
    }
    bytes.truncate(bytes.len() - padding);
    String::from_utf8(bytes).map_err(|_| "La respuesta descifrada no contiene texto UTF-8 válido.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_replaced_is_exactly_classified_without_server_details() {
        let protocol = Protocol::new().unwrap();
        let payload = Map::new();
        for code in ["aaa100083", "aaa100027"] {
            let error = protocol.decode_response(serde_json::json!({"returnCode":code,
                "errorMessage":"synthetic private server detail"}), "getColumnContents", &payload).unwrap_err();
            assert_eq!(error, SESSION_REPLACED_MESSAGE);
            assert!(is_session_expired_error(&error));
        }
        for code in ["aaa1000830", "AAA100083", "aaa100084", "aaa1000270", "aaa100028", "other"] {
            let error = protocol.decode_response(serde_json::json!({"returnCode":code,"errorMessage":"aaa100083"}),
                "getColumnContents", &payload).unwrap_err();
            assert!(!is_session_expired_error(&error));
        }
        assert!(!is_session_expired_error(&format!("prefix {SESSION_REPLACED_MESSAGE}")));
    }

    #[test]
    fn expired_playback_deadline_stops_before_dns_or_http(){
        let deadline=Instant::now();
        assert!(remaining_timeout(Some(deadline),Duration::from_secs(30)).is_err());
        let url=Url::parse("https://example.invalid/synthetic").unwrap();
        assert!(media_client_builder_until(&url,Some(deadline)).is_err());
        let mut protocol=Protocol::new().unwrap();
        let error=protocol.post_until("K0",serde_json::json!({"contentId":"synthetic"}),"",Some(deadline)).err().unwrap();
        assert!(error.contains("tardó demasiado"));
        assert_eq!(remaining_timeout(None,Duration::from_secs(3)).unwrap(),Duration::from_secs(3));
    }
    #[test]
    #[ignore]
    fn diagnose_public_dns_transport(){
        assert!(std::env::var("XUPER_DIAGNOSTIC").as_deref()==Ok("dns"),"Explicit public DNS diagnostic flag required");
        let mut report=Vec::new();
        for (name,endpoint,ipv4_only) in [("google-default","https://dns.google/resolve",false),("google-ipv4","https://dns.google/resolve",true),("cloudflare-default","https://cloudflare-dns.com/dns-query",false)] {
            let mut builder=Client::builder().connect_timeout(Duration::from_secs(3)).timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none());
            if ipv4_only{builder=builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED));}
            let started=Instant::now();
            let response=builder.build().unwrap().get(endpoint).query(&[("name","example.com"),("type","A")]).header("Accept","application/dns-json").send();
            let summary=match response {
                Ok(response)=>serde_json::json!({"provider":name,"elapsed_ms":started.elapsed().as_millis(),"http_status":response.status().as_u16()}),
                Err(error)=>{
                    let is_connect=error.is_connect();let is_timeout=error.is_timeout();
                    let error=error.without_url();
                    let mut messages=Vec::new();
                    let mut source=std::error::Error::source(&error);
                    while let Some(cause)=source {messages.push(cause.to_string());source=cause.source();}
                    serde_json::json!({"provider":name,"elapsed_ms":started.elapsed().as_millis(),"connect":is_connect,"timeout":is_timeout,"causes":messages})
                },
            };
            report.push(summary);
        }
        let path=std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("analysis/dns-transport.safe.json");
        std::fs::write(path,serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        println!("Public DNS transport diagnostic saved; only example.com queried.");
    }

    #[test]
    fn java_cipher_vectors() {
        let vectors: Value = serde_json::from_str(include_str!("../analysis/protocol-vectors.safe.json")).unwrap();
        let key = STANDARD.encode(b"0123456789abcdefghijklmn");
        for vector in vectors.as_array().unwrap() {
            let key=vector.get("key").and_then(Value::as_str).unwrap_or(&key);
            let plain = vector["plain"].as_str().unwrap();
            let expected = vector["encoded"].as_str().unwrap();
            assert_eq!(encrypt(plain, key).unwrap(), expected);
            assert_eq!(decrypt(expected, key).unwrap(), plain);
        }
    }

    #[test]
    fn rejects_invalid_envelopes() {
        let key = STANDARD.encode(b"0123456789abcdefghijklmn");
        for malformed in ["", "0", "zz", "4141", "414141414141414141414141"] {
            assert!(decrypt(malformed, &key).is_err());
        }
    }

    #[test]
    fn bundled_config_decodes_without_network() {
        let protocol = Protocol::new().unwrap();
        assert!(!protocol.endpoints.is_empty());
        assert!(protocol.config.source_apk_certificate_digest.len() == 59);
        assert_eq!(protocol.config.routes.get("o").unwrap(), "/api/portalCore/v3/snToken");
        for key in [&protocol.config.request_key,&protocol.config.response_key,&protocol.config.domain_key] {
            let encoded=encrypt("offline synthetic compatibility check",key).expect("Bundled key must prepare a request offline");
            let decoded=decrypt(&encoded,key).expect("Bundled key must decode an offline response");
            assert!(decoded=="offline synthetic compatibility check");
        }
    }

    #[test]
    fn dns_answers_are_bounded_to_public_a_records_and_ttl() {
        let response = serde_json::json!({"Status":0,"Answer":[
            {"type":5,"TTL":500,"data":"alias.example"},
            {"type":1,"TTL":300,"data":"1.1.1.1"},
            {"type":1,"TTL":120,"data":"8.8.8.8"},
            {"type":1,"TTL":200,"data":"1.1.1.1"},
            {"type":1,"TTL":5,"data":"127.0.0.1"},
            {"type":1,"TTL":5,"data":"10.0.0.1"},
            {"type":1,"TTL":5,"data":"100.64.0.1"},
            {"type":1,"TTL":5,"data":"169.254.169.254"},
            {"type":1,"TTL":5,"data":"224.0.0.1"},
            {"type":28,"TTL":5,"data":"2606:4700:4700::1111"}
        ]});
        let (addresses, ttl) = parse_dns_response(&response).unwrap();
        assert_eq!(addresses.len(), 2);
        assert_eq!(ttl, 120);
        assert!(addresses.iter().all(|address|address.port()==0));
        assert!(parse_dns_response(&serde_json::json!({"Status":3,"Answer":response["Answer"]})).is_err());
        assert!(parse_dns_response(&serde_json::json!({"Status":0,"Answer":[{"type":1,"TTL":300,"data":"127.0.0.1"}]})).is_err());
        assert!(parse_dns_response(&serde_json::json!({"Status":0,"Answer":[{"type":1,"data":"1.1.1.1"}]})).is_err());
        let (_, ttl)=parse_dns_response(&serde_json::json!({"Status":0,"Answer":[{"type":1,"TTL":0,"data":"1.1.1.1"}]})).unwrap();
        assert_eq!(ttl,0);
        let (_,ttl)=parse_dns_response(&serde_json::json!({"Status":0,"Answer":[{"type":5,"TTL":30,"data":"alias.example"},{"type":1,"TTL":300,"data":"1.1.1.1"}]})).unwrap();
        assert_eq!(ttl,30);
    }
}
