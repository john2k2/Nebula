use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_page: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<Episode>,
    #[serde(default, flatten)]
    pub presentation: Presentation,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Presentation {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub director: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub backdrop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub year: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cast: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub genres: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub seasons: Vec<Season>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Season { pub id: String, pub number: i64 }

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub poster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub duration: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub title: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_title: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct CatalogPage {
    pub items: Vec<Item>,
    pub page: u64,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub installation_id: String,
    #[serde(default)]
    pub sn: String,
    #[serde(default)]
    pub sn_token: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_token: String,
    #[serde(default)]
    pub portal_code: String,
    #[serde(default)]
    pub has_pay: String,
    #[serde(default)]
    pub user_identity: String,
}

#[derive(Clone)]
pub struct ItemContext {
    pub item: Item,
    pub source: Value,
    pub column_id: Option<i64>,
    pub series_content_id: Option<String>,
    pub episode_number: Option<i64>,
}

/// Private playback state. Licenses must never be serialized into the UI,
/// favorites, diagnostic output, or a log.
pub struct PlaybackCandidate {
    pub media_code: String,
    pub license: String,
    pub tag: String,
    pub quality: String,
    pub format: String,
    pub audio_info: String,
}

fn playback_tags(slb: &Value, kind: &str) -> Result<Vec<String>, String> {
    // q5.i.d/e/f: group by business, sort serial_number, then use the first
    // entry's CdnUrl tags. Empty URLs do not authorize a media candidate.
    let entry = slb.get("cdn_list").and_then(Value::as_array)
        .ok_or("No se ha recibido la configuración del reproductor.")?
        .iter().filter(|entry| text(entry, "tag").as_deref() == Some(kind))
        .min_by_key(|entry| integer(entry, "serial_number").unwrap_or(0))
        .ok_or("El servidor no ofrece reproducción para este tipo de contenido.")?;
    Ok(entry.get("url_list").and_then(Value::as_array).into_iter().flatten()
        .filter(|row| text(row, "url").is_some()).filter_map(|row| text(row, "tag")).collect())
}

/// Its initial 480p/por choice follows the observed Java fallback branches;
/// it does not import or claim to match preferences saved on an Android device.
pub struct VodPreferences<'a> {
    pub quality: &'a str,
    pub audio_language: &'a str,
}

impl Default for VodPreferences<'static> {
    fn default() -> Self { Self { quality: "480p", audio_language: "por" } }
}

/// Private selection; neither licenses nor the selected media ID enter the UI.
pub struct VodSelection {
    pub selected_media_code: String,
    pub candidates: Vec<PlaybackCandidate>,
    pub selected: VodSelectedOptions,
}

/// These are the only playback-selection fields allowed into desktop IPC.
/// Media IDs, licenses, URLs and authorization tags stay in VodSelection.
#[derive(Clone, Serialize)]
pub struct VodSelectedOptions {
    pub quality: String,
    pub audio_language: Option<String>,
    pub audio_track_index: Option<usize>,
}

#[derive(Clone, Serialize)]
pub struct VodAudioOption { pub value: String, pub label: String }

#[derive(Clone, Serialize)]
pub struct VodQualityOption {
    pub value: String,
    pub label: String,
    pub audio_languages: Vec<VodAudioOption>,
}

#[derive(Serialize)]
pub struct VodPlaybackOptions {
    pub id: String,
    pub kind: String,
    pub qualities: Vec<VodQualityOption>,
    pub selected: Option<VodSelectedOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

pub fn valid_audio_language(value: &str) -> bool {
    if !(2..=16).contains(&value.len()) { return false; }
    let mut parts=value.split(['-','_']);
    let Some(language)=parts.next() else{return false;};
    (2..=3).contains(&language.len()) && language.bytes().all(|byte|byte.is_ascii_alphabetic())
        && parts.all(|part|(2..=8).contains(&part.len()) && part.bytes().all(|byte|byte.is_ascii_alphanumeric()))
}

pub(crate) fn audio_label(value: &str) -> String {
    match value {
        "por" | "pt" => "Portugués".into(), "eng" | "en" => "Inglés".into(),
        "spa" | "es" => "Español".into(), "fra" | "fre" | "fr" => "Francés".into(),
        "deu" | "ger" | "de" => "Alemán".into(), "ita" | "it" => "Italiano".into(),
        "jpn" | "ja" => "Japonés".into(), "kor" | "ko" => "Coreano".into(),
        "zho" | "chi" | "zh" => "Chino".into(), "und" => "Sin especificar".into(),
        _ => value.to_ascii_uppercase(),
    }
}

/// Normal playback uses authType=null in Java, so Gson omits it. A non-series
/// title nevertheless sends seriesContentId as an explicit, non-null "".
pub fn vod_start_payload(content_id: &str, column_id: Option<i64>, series_content_id: Option<&str>, episode_number: Option<i64>) -> Value {
    let mut body = serde_json::json!({
        "type": if column_id.is_some() { "0" } else { "1" },
        "contentId": content_id,
        "seriesContentId": series_content_id.unwrap_or(""),
        "startTime": 0
    });
    if let Some(column) = column_id { body["columnId"] = serde_json::json!(column); }
    if let Some(episode) = episode_number { body["episodeNumberList"] = serde_json::json!([episode]); }
    body
}

pub fn vod_candidates(play: &Value, slb: &Value, content_id: &str, episode_number: Option<i64>) -> Result<Vec<PlaybackCandidate>, String> {
    Ok(select_vod_candidates(play, slb, content_id, episode_number, &VodPreferences::default())?.candidates)
}

pub fn select_vod_candidates(play: &Value, slb: &Value, content_id: &str, episode_number: Option<i64>, preferences: &VodPreferences<'_>) -> Result<VodSelection, String> {
    let groups = vod_quality_groups(play, content_id, episode_number)?;
    let candidates = authorized_vod_candidates(&groups, slb)?;
    choose_vod_candidates(&groups, candidates, preferences)
}

/// Explicit user choices must exist at the chosen quality; only omitted
/// preferences use the original Java fallback. The exposed options are made
/// from the same first-license/tag-authorized candidates used for playback.
pub fn vod_options_and_selection(play: &Value, slb: &Value, content_id: &str, episode_number: Option<i64>, quality: Option<&str>, audio_language: Option<&str>) -> Result<(Vec<VodQualityOption>, VodSelection), String> {
    let groups = vod_quality_groups(play, content_id, episode_number)?;
    let candidates = authorized_vod_candidates(&groups, slb)?;
    let mut qualities = Vec::new();
    for quality in ["480p", "720p", "1080p"] {
        let matching: Vec<_> = candidates.iter().filter(|candidate| candidate.quality == quality).collect();
        if matching.is_empty() { continue; }
        let mut languages: Vec<VodAudioOption> = Vec::new();
        for candidate in matching {
            for language in candidate.audio_info.split(',').filter(|language| valid_audio_language(language)) {
                if !languages.iter().any(|option| option.value == language) {
                    languages.push(VodAudioOption { value:language.into(), label:audio_label(language) });
                }
            }
        }
        qualities.push(VodQualityOption { value:quality.into(), label:quality.into(), audio_languages:languages });
    }
    if quality.is_some_and(|requested| !qualities.iter().any(|option| option.value == requested)) {
        return Err("La calidad elegida no está disponible para este episodio.".into());
    }
    let defaults = VodPreferences::default();
    let preferences = VodPreferences { quality:quality.unwrap_or(defaults.quality), audio_language:audio_language.unwrap_or(defaults.audio_language) };
    let selection = choose_vod_candidates(&groups, candidates, &preferences)?;
    if let Some(requested) = audio_language {
        let available = qualities.iter().find(|option| option.value == selection.selected.quality)
            .is_some_and(|quality| quality.audio_languages.iter().any(|option| option.value == requested));
        if !valid_audio_language(requested) || !available {
            return Err("El idioma elegido no está disponible en esta calidad.".into());
        }
    }
    Ok((qualities, selection))
}

pub(crate) fn vod_episode<'a>(play: &'a Value, content_id: &str, episode_number: Option<i64>) -> Result<&'a Value, String> {
    let episodes = play.get("episodeList").and_then(Value::as_array)
        .ok_or("El servidor no devolvió los episodios autorizados.")?;
    episodes.iter().find(|episode| text(episode, "programContentId").as_deref() == Some(content_id))
        .or_else(|| episode_number.and_then(|number| episodes.iter().find(|episode| integer(episode, "episodeNumber") == Some(number))))
        .or_else(|| if episode_number.is_none() { episodes.first() } else { None })
        .ok_or_else(|| "El servidor no autorizó el episodio seleccionado.".into())
}

fn vod_quality_groups<'a>(play: &'a Value, content_id: &str, episode_number: Option<i64>) -> Result<Vec<(&'a str, &'a Value)>, String> {
    let episode = vod_episode(play, content_id, episode_number)?;
    // j6.p2.w: only these three nonempty quality groups enter the playable
    // map. A later nonempty duplicate replaces the earlier value; an empty
    // duplicate does not erase it. Keep response insertion order in Windows.
    let mut groups: Vec<(&str, &Value)> = Vec::new();
    for group in episode.get("totalMovieList").and_then(Value::as_array).into_iter().flatten() {
        let Some(quality) = group.get("quality").and_then(Value::as_str) else { continue; };
        if !["480p", "720p", "1080p"].contains(&quality)
            || !group.get("movieList").and_then(Value::as_array).is_some_and(|movies| !movies.is_empty()) { continue; }
        if let Some(existing) = groups.iter_mut().find(|(name, _)| *name == quality) {
            existing.1 = group;
        } else {
            groups.push((quality, group));
        }
    }
    Ok(groups)
}

fn authorized_vod_candidates(groups: &[(&str, &Value)], slb: &Value) -> Result<Vec<PlaybackCandidate>, String> {
    let tags = playback_tags(slb, "vod")?;
    let mut candidates = Vec::new();
    for (quality, group) in groups {
        for movie in group.get("movieList").and_then(Value::as_array).into_iter().flatten() {
            // B0 uses the first License, never a later license as fallback.
            let Some(first) = movie.get("licenseList").and_then(Value::as_array).and_then(|rows| rows.first()) else { continue; };
            let Some(tag) = text(first, "tag").filter(|tag| tags.contains(tag)) else { continue; };
            let (Some(media_code), Some(license)) = (text(movie, "contentId"), text(first, "license")) else { continue; };
            let audio_info = text(movie, "audioInfo").unwrap_or_default();
            if candidates.len() >= 64 { return Err("El servidor devolvió demasiadas opciones de reproducción.".into()); }
            candidates.push(PlaybackCandidate { media_code, license, tag, audio_info,
                quality: (*quality).into(), format: text(movie, "videoFormat").unwrap_or_default() });
        }
    }
    Ok(candidates)
}

fn choose_vod_candidates(groups: &[(&str, &Value)], mut candidates: Vec<PlaybackCandidate>, preferences: &VodPreferences<'_>) -> Result<VodSelection, String> {
    // REA j6.p2.T: preference and absence fallback are independent of server
    // ordering. Explicit unknown values are rejected by the options entrypoint.
    let quality_order = match preferences.quality {
        "720p" => ["720p", "480p", "1080p"],
        "1080p" => ["1080p", "720p", "480p"],
        _ => ["480p", "720p", "1080p"],
    };
    let (selected_quality, selected_group) = quality_order.iter()
        .find_map(|quality| groups.iter().find(|(name, _)| name == quality))
        .ok_or("El servidor no ofreció una versión compatible de este título.")?;
    let first_movie = selected_group.get("movieList").and_then(Value::as_array).and_then(|movies| movies.first())
        .ok_or("El servidor no ofreció una versión compatible de este título.")?;
    let mut selected_media_code = candidates.iter().find(|candidate| candidate.quality == *selected_quality
        && candidate.audio_info.split(',').any(|language| language == preferences.audio_language))
        .map(|candidate| candidate.media_code.clone())
        .unwrap_or_else(|| text(first_movie, "contentId").unwrap_or_default());
    // Program.media selects by Media.name. Ranger then filters by exact full
    // Media.lang (audioInfo), not membership in a comma-separated language set.
    let selected_audio = candidates.iter().find(|candidate| candidate.media_code == selected_media_code)
        .ok_or("El servidor no autorizó la pista seleccionada para este título.")?.audio_info.clone();
    // The TS representation can play in VLC 3 over HTTP while exposing neither
    // duration nor seeking. Prefer an authorized MP4 of the very same quality
    // and complete audio bundle. Never trade the user's resolution or language
    // for a different container, and keep TS when no equivalent MP4 exists.
    if let Some(file)=candidates.iter().find(|candidate|candidate.quality==*selected_quality
        && candidate.audio_info==selected_audio && candidate.format.eq_ignore_ascii_case("mp4")) {
        selected_media_code=file.media_code.clone();
    }
    // REA j6.p2.B0 stores the token's position in audioInfo, and k8.e.D0
    // selects the corresponding engine track. It is not always track zero.
    let selected_track = selected_audio.split(',').enumerate().find(|(_, language)| *language == preferences.audio_language)
        .or_else(|| selected_audio.split(',').enumerate().next())
        .filter(|(_, language)| valid_audio_language(language));
    let selected = VodSelectedOptions { quality:(*selected_quality).into(),
        audio_language:selected_track.map(|(_, language)| language.to_string()),
        audio_track_index:selected_track.map(|(index, _)| index) };
    candidates.retain(|candidate| candidate.audio_info == selected_audio);
    Ok(VodSelection { selected_media_code, candidates, selected })
}

pub fn live_candidates(channel: &Value, slb: &Value) -> Result<Vec<PlaybackCandidate>, String> {
    let tags = playback_tags(slb, "live")?;
    let mut result = Vec::new();
    for address in channel.get("liveAddressList").and_then(Value::as_array).into_iter().flatten() {
        // k6.g0.t uses LiveAddress.playCode, not Channel.channelCode.
        let Some(tag) = text(address, "tag").filter(|tag| tags.contains(tag)) else { continue; };
        let (Some(media_code), Some(license)) = (text(address, "playCode"), text(address, "license")) else { continue; };
        if result.len() >= 64 { return Err("El servidor devolvió demasiadas señales para este canal.".into()); }
        result.push(PlaybackCandidate { media_code, license, tag,
            quality: text(address, "quality").unwrap_or_default(), format: text(address, "AVFormat").unwrap_or_default(), audio_info: String::new() });
    }
    if result.is_empty() { return Err("El servidor no ofreció una señal compatible para este canal.".into()); }
    Ok(result)
}

pub fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(crate::protocol::scalar).filter(|s| !s.is_empty())
}

pub fn integer(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

pub fn http_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url.into())
}

pub fn normalize_item(source: &Value, kind: &str) -> Option<Item> {
    let id = if kind == "live" { text(source, "channelCode") } else { text(source, "contentId") }?;
    let title = text(source, "name").or_else(|| text(source, "alias"))?;
    let poster = text(source, "posterUrl").and_then(|s| http_url(&s)).or_else(|| {
        source.get("posterList")?.as_array()?.iter().find_map(|p| text(p, "fileUrl").and_then(|u| http_url(&u)))
    });
    let source_type = text(source,"type").filter(|s|s=="0" || s=="1").or_else(||match text(source,"programType").as_deref(){
        Some("movie")=>Some("1".into()),Some("teleplay")=>Some("0".into()),_=>None,
    });
    let mut seasons:Vec<_> = source.get("sameSeasonSeriesList").and_then(Value::as_array).into_iter().flatten().take(200)
        .filter_map(|season| Some(Season { id:text(season,"contentId")?, number:integer(season,"seasonNumber")? }))
        .filter(|season| season.number >= 0 && season.number <= 10000).collect();
    seasons.sort_by_key(|season|season.number); seasons.dedup_by(|left,right|left.id==right.id);
    let presentation=Presentation { backdrop:landscape_poster(source),
        score:source.get("score").and_then(|v|v.as_f64().or_else(||v.as_str()?.parse().ok()))
            .filter(|score|score.is_finite() && *score>0.0 && *score<=10.0),
        country:text(source,"originalCountry"), language:text(source,"language"), director:text(source,"director"),
        year:text(source,"releaseTime").and_then(|date| date.get(..4).filter(|year|year.chars().all(|c|c.is_ascii_digit())).map(str::to_string)),
        duration:text(source,"duration"), cast:text(source,"actorDisplay"), genres:text(source,"tags"), seasons };
    Some(Item { id, title, kind: kind.into(), poster, source_type, source_category:None, source_page:None, description: text(source, "description"), episodes: vec![], presentation })
}

pub(crate) fn landscape_poster(source:&Value)->Option<String> {
    for role in ["background","banner","stage"] {
        if let Some(url)=source.get("posterList").and_then(Value::as_array).into_iter().flatten()
            .filter(|poster|text(poster,"fileType").as_deref()==Some(role))
            .find_map(|poster|text(poster,"fileUrl").and_then(|url|http_url(&url))) { return Some(url); }
    }
    source.get("posterList")?.as_array()?.iter().find_map(|poster| {
        let size=text(poster,"size")?;
        let dimensions:Vec<_>=size.split(['x','X','*','×']).filter_map(|part|part.trim().parse::<u32>().ok()).collect();
        if dimensions.len()!=2 || dimensions[1]==0 || dimensions[0] as f64 / (dimensions[1] as f64) < 1.4 { return None; }
        text(poster,"fileUrl").and_then(|url|http_url(&url))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn movie_filters_keep_source_metadata_and_missing_scores_are_unknown(){
        let item=normalize_item(&json!({"contentId":"x","name":"Example","score":"8.6","originalCountry":"AR","language":"es","director":"Example director"}),"vod").unwrap();
        assert_eq!(item.presentation.score,Some(8.6));assert_eq!(item.presentation.country.as_deref(),Some("AR"));
        assert_eq!(item.presentation.language.as_deref(),Some("es"));assert_eq!(item.presentation.director.as_deref(),Some("Example director"));
        assert_eq!(normalize_item(&json!({"contentId":"s","name":"Series","programType":"teleplay"}),"vod").unwrap().source_type.as_deref(),Some("0"));
        for score in [json!(0),json!(-1),json!(11),json!("NaN"),Value::Null]{
            assert!(normalize_item(&json!({"contentId":"x","name":"Example","score":score}),"vod").unwrap().presentation.score.is_none());
        }
    }
    #[test]
    fn presentation_preserves_real_seasons_and_semantic_background_without_invented_season() {
        let item=normalize_item(&json!({"contentId":"series","name":"Serie","type":"0","releaseTime":"2024-03-12",
            "sameSeasonSeriesList":[{"contentId":"s2","seasonNumber":2},{"contentId":"s1","seasonNumber":1}],
            "posterList":[{"fileType":"poster","fileUrl":"https://example.invalid/poster.jpg"},
                {"fileType":"background","fileUrl":"https://example.invalid/background.jpg"}]}),"vod").unwrap();
        assert_eq!(item.presentation.seasons.iter().map(|season|season.number).collect::<Vec<_>>(),vec![1,2]);
        assert_eq!(item.presentation.backdrop.as_deref(),Some("https://example.invalid/background.jpg"));
        assert_eq!(item.presentation.year.as_deref(),Some("2024"));
        assert!(normalize_item(&json!({"contentId":"film","name":"Película"}),"vod").unwrap().presentation.seasons.is_empty());
    }

    #[test]
    fn maps_observed_catalog_shape_and_rejects_non_web_urls() {
        let item = normalize_item(&json!({"contentId":"synthetic-1","name":"Example","posterList":[{"fileUrl":"javascript:alert(1)"},{"fileUrl":"https://example.invalid/poster.jpg"}]}),"vod").unwrap();
        assert_eq!(item.id,"synthetic-1");
        assert_eq!(item.poster.as_deref(), Some("https://example.invalid/poster.jpg"));
        assert!(http_url("file:///private").is_none());
        assert!(http_url("https://user:password@example.invalid/").is_none());
        assert!(normalize_item(&json!({"name":"Missing ID"}),"vod").is_none());
    }

    #[test]
    fn playback_selection_uses_authorized_episode_and_native_media_ids() {
        let slb = json!({"cdn_list":[
            {"tag":"vod","serial_number":2,"url_list":[{"tag":"other","url":"synthetic-other-auth"}]},
            {"tag":"vod","serial_number":1,"url_list":[{"tag":"allowed","url":"synthetic-auth"}]},
            {"tag":"live","serial_number":1,"url_list":[{"tag":"live-tag","url":"synthetic-auth"}]}
        ]});
        let play = json!({"episodeList":[
            {"programContentId":"episode-one","episodeNumber":1,"totalMovieList":[]},
            {"programContentId":"episode-two","episodeNumber":2,"totalMovieList":[{"quality":"480p","movieList":[
                {"contentId":"media-two","videoFormat":"mp4","licenseList":[{"tag":"allowed","license":"synthetic-license"}]},
                {"contentId":"not-first-license","licenseList":[{"tag":"other","license":"synthetic-one"},{"tag":"allowed","license":"synthetic-two"}]}
            ]}]}
        ]});
        let candidates = vod_candidates(&play, &slb, "episode-two", Some(2)).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].media_code, "media-two");
        assert_eq!(candidates[0].quality, "480p");
        assert!(vod_candidates(&play, &slb, "absent-episode", Some(3)).is_err());
        let live = live_candidates(&json!({"channelCode":"channel-list-id","liveAddressList":[
            {"playCode":"live-media-code","license":"synthetic-live-license","tag":"live-tag","AVFormat":"hls","quality":"1"}
        ]}), &slb).unwrap();
        assert_eq!(live[0].media_code, "live-media-code");
        assert_eq!(live[0].format, "hls");
    }

    fn selection_slb() -> Value {
        json!({"cdn_list":[{"tag":"vod","serial_number":1,"url_list":[{"tag":"allowed","url":"synthetic-auth"}]}]})
    }
    fn selection_movie(id: &str, audio: &str) -> Value {
        json!({"contentId":id,"audioInfo":audio,"videoFormat":"mp4","licenseList":[{"tag":"allowed","license":"synthetic-license"}]})
    }
    fn selection_play(groups: Vec<Value>) -> Value {
        json!({"episodeList":[{"programContentId":"synthetic-title","episodeNumber":1,"totalMovieList":groups}]})
    }

    #[test]
    fn start_play_keeps_empty_series_id_but_omits_absent_optional_fields() {
        let movie = vod_start_payload("synthetic-movie", None, None, None);
        // Inspect the actual serialized request shape, not merely a defaulted
        // lookup (which would hide null/absent versus an empty string).
        let serialized: Value = serde_json::from_slice(&serde_json::to_vec(&movie).unwrap()).unwrap();
        assert_eq!(serialized, json!({"type":"1","contentId":"synthetic-movie","seriesContentId":"","startTime":0}));
        assert!(serialized.as_object().unwrap().contains_key("seriesContentId"));
        assert!(!serialized.as_object().unwrap().contains_key("authType"));
        assert_eq!(vod_start_payload("synthetic-episode", Some(7), Some("synthetic-series"), Some(3)),
            json!({"type":"0","columnId":7,"contentId":"synthetic-episode","seriesContentId":"synthetic-series","episodeNumberList":[3],"startTime":0}));
    }

    #[test]
    fn selected_audio_filters_the_full_bundle_across_qualities() {
        let play = selection_play(vec![
            json!({"quality":"720p","movieList":[selection_movie("same-bundle-720", "por,eng"),selection_movie("por-only-720", "por")]}),
            json!({"quality":"480p","movieList":[selection_movie("english-first-480", "eng"),selection_movie("selected-480", "por,eng"),selection_movie("reordered-480", "eng,por")]}),
            json!({"quality":"2160p","movieList":[selection_movie("unsupported", "por,eng")]})
        ]);
        let selection = select_vod_candidates(&play, &selection_slb(), "synthetic-title", None, &VodPreferences::default()).unwrap();
        assert_eq!(selection.selected_media_code, "selected-480");
        assert_eq!(selection.candidates.iter().map(|candidate| candidate.media_code.as_str()).collect::<Vec<_>>(), vec!["same-bundle-720", "selected-480"]);
        let english = select_vod_candidates(&play, &selection_slb(), "synthetic-title", None,
            &VodPreferences { quality: "480p", audio_language: "eng" }).unwrap();
        assert_eq!(english.selected_media_code, "english-first-480");
        assert_eq!(english.candidates.len(), 1);
    }

    #[test]
    fn mp4_preference_preserves_quality_audio_bundle_and_first_license() {
        let mut ts=selection_movie("full-hd-ts","spa,eng");ts["videoFormat"]=json!("ts");
        let mut blocked=selection_movie("blocked-mp4","spa,eng");blocked["licenseList"]=json!([{"tag":"blocked","license":"not-authorized"}]);
        let play=selection_play(vec![json!({"quality":"1080p","movieList":[ts,blocked,selection_movie("different-audio","spa"),selection_movie("full-hd-mp4","spa,eng")]}),
            json!({"quality":"720p","movieList":[selection_movie("lower-mp4","spa,eng")]})]);
        let selected=select_vod_candidates(&play,&selection_slb(),"synthetic-title",None,&VodPreferences{quality:"1080p",audio_language:"eng"}).unwrap();
        assert_eq!(selected.selected_media_code,"full-hd-mp4");assert_eq!(selected.selected.quality,"1080p");assert_eq!(selected.selected.audio_language.as_deref(),Some("eng"));
        let mut only_ts=play.clone();only_ts["episodeList"][0]["totalMovieList"][0]["movieList"].as_array_mut().unwrap().pop();
        let selected=select_vod_candidates(&only_ts,&selection_slb(),"synthetic-title",None,&VodPreferences{quality:"1080p",audio_language:"eng"}).unwrap();
        assert_eq!(selected.selected_media_code,"full-hd-ts");
    }

    #[test]
    fn duplicate_quality_and_missing_quality_follow_java_rules() {
        let play = selection_play(vec![
            json!({"quality":"480p","movieList":[selection_movie("replaced", "por")]}),
            json!({"quality":"480p","movieList":[selection_movie("last-nonempty", "por")]}),
            json!({"quality":"480p","movieList":[]}),
            json!({"quality":"1080p","movieList":[selection_movie("full-hd", "por")]})
        ]);
        let selection = select_vod_candidates(&play, &selection_slb(), "synthetic-title", None,
            &VodPreferences { quality: "720p", audio_language: "por" }).unwrap();
        assert_eq!(selection.selected_media_code, "last-nonempty");
        assert!(!selection.candidates.iter().any(|candidate| candidate.media_code == "replaced"));
        let alternatives = [
            ("480p", vec!["1080p", "720p"], "720p"),
            ("720p", vec!["1080p", "480p"], "480p"),
            ("1080p", vec!["480p", "720p"], "720p")
        ];
        for (preference, qualities, expected) in alternatives {
            let play = selection_play(qualities.iter().map(|quality| json!({"quality":quality,"movieList":[selection_movie(quality,"por")]})).collect());
            let selection = select_vod_candidates(&play, &selection_slb(), "synthetic-title", None,
                &VodPreferences { quality: preference, audio_language: "por" }).unwrap();
            assert_eq!(selection.selected_media_code, expected);
        }
    }

    #[test]
    fn missing_audio_uses_first_movie_and_never_a_later_license() {
        let play = selection_play(vec![json!({"quality":"480p","movieList":[selection_movie("first", "eng"),selection_movie("second", "spa")]})]);
        let selection = select_vod_candidates(&play, &selection_slb(), "synthetic-title", None, &VodPreferences::default()).unwrap();
        assert_eq!(selection.selected_media_code, "first");
        assert_eq!(selection.candidates.len(), 1);
        let mut blocked = play;
        blocked["episodeList"][0]["totalMovieList"][0]["movieList"][0]["licenseList"] = json!([
            {"tag":"unavailable","license":"synthetic-not-allowed"},
            {"tag":"allowed","license":"synthetic-not-a-fallback"}
        ]);
        // Fail closed if Java's first-movie fallback has no allowed Media;
        // do not silently choose another language or a second license.
        assert!(select_vod_candidates(&blocked, &selection_slb(), "synthetic-title", None, &VodPreferences::default()).is_err());
        let unknown = selection_play(vec![json!({"quality":"2160p","movieList":[selection_movie("unknown-quality","por")]})]);
        assert!(vod_candidates(&unknown, &selection_slb(), "synthetic-title", None).is_err());
    }

    #[test]
    fn vod_options_use_real_authorized_languages_and_the_selected_track_index() {
        let mut blocked=selection_movie("blocked-media", "jpn");
        blocked["licenseList"]=json!([{"tag":"blocked","license":"private-first-license"},
            {"tag":"allowed","license":"private-second-license-not-a-fallback"}]);
        let play=selection_play(vec![
            json!({"quality":"720p","movieList":[selection_movie("private-720", "por,eng")]}),
            json!({"quality":"480p","movieList":[selection_movie("private-selected", "por,eng,spa"),blocked]}),
            json!({"quality":"2160p","movieList":[selection_movie("private-unsupported", "fra")]})
        ]);
        let (qualities,selection)=vod_options_and_selection(&play,&selection_slb(),"synthetic-title",None,Some("480p"),Some("spa")).unwrap();
        assert_eq!(qualities.iter().map(|quality|quality.value.as_str()).collect::<Vec<_>>(),vec!["480p","720p"]);
        assert_eq!(qualities[0].audio_languages.iter().map(|language|language.value.as_str()).collect::<Vec<_>>(),vec!["por","eng","spa"]);
        assert_eq!(qualities[1].audio_languages.iter().map(|language|language.value.as_str()).collect::<Vec<_>>(),vec!["por","eng"]);
        assert_eq!(selection.selected_media_code,"private-selected");
        assert_eq!(selection.selected.audio_language.as_deref(),Some("spa"));
        assert_eq!(selection.selected.audio_track_index,Some(2));
        assert_eq!(selection.candidates.len(),1);
        let exposed=serde_json::to_value(VodPlaybackOptions{id:"synthetic-title".into(),kind:"vod".into(),qualities,
            selected:Some(selection.selected),notice:None}).unwrap();
        let wire=exposed.to_string();
        for forbidden in ["private-","synthetic-license","synthetic-auth","license","media_code","url","token","tag"]{
            assert!(!wire.contains(forbidden),"Options must contain only display metadata");
        }
        assert_eq!(exposed["selected"]["audio_track_index"],2);
    }

    #[test]
    fn vod_options_reject_explicit_unavailable_combinations_but_keep_default_fallback() {
        let play=selection_play(vec![
            json!({"quality":"480p","movieList":[selection_movie("english","eng")]}),
            json!({"quality":"720p","movieList":[selection_movie("portuguese","por")]})
        ]);
        let (_,default)=vod_options_and_selection(&play,&selection_slb(),"synthetic-title",None,None,None).unwrap();
        assert_eq!(default.selected.quality,"480p");
        assert_eq!(default.selected.audio_language.as_deref(),Some("eng"));
        assert_eq!(default.selected.audio_track_index,Some(0));
        let (_,chosen)=vod_options_and_selection(&play,&selection_slb(),"synthetic-title",None,Some("720p"),Some("por")).unwrap();
        assert_eq!(chosen.selected_media_code,"portuguese");
        for (quality,audio) in [(Some("1080p"),None),(Some("480p"),Some("por")),(Some("720p"),Some("spa")),(Some("2160p"),None)]{
            assert!(vod_options_and_selection(&play,&selection_slb(),"synthetic-title",None,quality,audio).is_err());
        }
        let missing_default=selection_play(vec![json!({"quality":"720p","movieList":[selection_movie("available","spa")]})]);
        let (_,fallback)=vod_options_and_selection(&missing_default,&selection_slb(),"synthetic-title",None,None,None).unwrap();
        assert_eq!(fallback.selected.quality,"720p");
        assert_eq!(fallback.selected.audio_language.as_deref(),Some("spa"));
    }

    #[test]
    fn vod_options_do_not_invent_language_metadata_or_cross_episodes() {
        let mut play=selection_play(vec![json!({"quality":"480p","movieList":[selection_movie("no-language","")]})]);
        play["episodeList"].as_array_mut().unwrap().push(json!({"programContentId":"episode-two","episodeNumber":2,
            "totalMovieList":[{"quality":"1080p","movieList":[selection_movie("other-episode","jpn")]}]}));
        let (qualities,selection)=vod_options_and_selection(&play,&selection_slb(),"synthetic-title",Some(1),None,None).unwrap();
        assert_eq!(qualities.len(),1);
        assert!(qualities[0].audio_languages.is_empty());
        assert!(selection.selected.audio_language.is_none() && selection.selected.audio_track_index.is_none());
        assert!(vod_options_and_selection(&play,&selection_slb(),"synthetic-title",Some(1),None,Some("jpn")).is_err());
        assert!(vod_options_and_selection(&play,&selection_slb(),"missing",Some(3),None,None).is_err());
        assert!(valid_audio_language("pt-BR"));
        for malformed in ["", "http://secret.invalid", "secret-token", "por,eng", "a", "spa "]{
            assert!(!valid_audio_language(malformed));
        }
    }

}
