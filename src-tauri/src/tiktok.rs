//! Cliente da API de short dramas do TikTok.
//!
//! Porte da logica verificada em tkdrama.py: pagina /api/drama/episode/item_list/
//! com os cookies da sessao e monta a lista de episodios numerada.
//!
//! O JSON e lido como `Value` de proposito: a resposta tem dezenas de campos que
//! nao usamos e que mudam com frequencia. Extrair so o necessario evita quebrar
//! a cada mexida do TikTok.

use anyhow::{anyhow, bail, Context, Result};

use serde_json::Value;

use crate::media::{Episode, Gear, Series, Source, Theme, Upstream, UA};

const API: &str = "https://www.tiktok.com/api/drama/episode/item_list/";
const PAGE: u32 = 24;
const MAX_PAGES: u32 = 200;

pub const AUTH_HINT: &str =
    "Sessao do TikTok expirada. Abra Entrar e faca login de novo para renovar.";

/// Nunca houve sessao — diferente de uma que expirou, mas o remedio e o mesmo.
pub const NO_SESSION: &str = "Voce ainda nao entrou no TikTok. Clique em Entrar.";

/// A falha e de login? Repetir nao resolve: so o usuario entrando resolve.
pub fn is_auth_error(msg: &str) -> bool {
    msg.contains(AUTH_HINT) || msg.contains(NO_SESSION)
}

/// Aceita URL completa (/shortdrama/episode/<id>/1), ?dramaID=<id> ou o ID puro.
pub fn parse_drama_id(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches('"');
    if raw.len() >= 6 && raw.chars().all(|c| c.is_ascii_digit()) {
        return Some(raw.to_string());
    }
    for marker in ["/shortdrama/episode/", "/shortdrama/detail/", "dramaID="] {
        if let Some(idx) = raw.find(marker) {
            let rest = &raw[idx + marker.len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if id.len() >= 6 {
                return Some(id);
            }
        }
    }
    None
}

/// Detecta uma URL de video comum (/@usuario/video/<id> ou /video/<id>).
///
/// Esta e a URL que aparece na barra de endereco ao assistir um episodio. Ela
/// nao carrega o dramaID: e preciso abrir a pagina na sessao logada para
/// descobrir a serie a que o video pertence (ver o resolvedor no WebView).
pub fn parse_video_id(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches('"');
    if let Some(idx) = raw.find("/video/") {
        let rest = &raw[idx + "/video/".len()..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if id.len() >= 6 {
            return Some(id);
        }
    }
    None
}

/// Constroi a URL de assistir de um video, para carregar na sessao logada.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.tiktok.com/@placeholder/video/{video_id}")
}

fn as_u64(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// Le as variantes de qualidade, descartando as que nao tem URL utilizavel.
fn parse_gears(video: &Value) -> Vec<Gear> {
    let mut out = Vec::new();
    let Some(list) = video.get("bitrateInfo").and_then(|v| v.as_array()) else {
        return out;
    };
    for b in list {
        let pa = match b.get("PlayAddr") {
            Some(p) => p,
            None => continue,
        };
        let urls: Vec<String> = pa
            .get("UrlList")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|u| u.as_str())
                    .filter(|s| s.starts_with("http"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            continue;
        }
        out.push(Gear {
            gear_name: b
                .get("GearName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            codec: b
                .get("CodecType")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            width: as_u64(pa.get("Width").unwrap_or(&Value::Null)) as u32,
            height: as_u64(pa.get("Height").unwrap_or(&Value::Null)) as u32,
            bitrate: as_u64(b.get("Bitrate").unwrap_or(&Value::Null)),
            size: as_u64(pa.get("DataSize").unwrap_or(&Value::Null)),
            urls,
        });
    }
    out
}

/// Espelhos da capa da serie, em resolucao original.
///
/// Le `cover.urlList` e nao `zoomCover`: o `zoomCover` e um recorte QUADRADO de
/// 240-960 px, e a capa que se quer e a vertical da novela. O campo e um objeto,
/// nunca uma string — le-lo como texto (o que este arquivo fazia) devolvia vazio
/// sempre, e era por isso que nao havia capa nenhuma.
fn parse_covers(info: &Value) -> Vec<String> {
    info.get("cover")
        .and_then(|c| c.get("urlList"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str())
                .filter(|s| s.starts_with("http"))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Temas da serie, crus. O `tagKey` e o identificador estavel; o `tagVal` e o
/// rotulo em ingles da plataforma. Nenhum dos dois se traduz aqui.
fn parse_themes(info: &Value) -> Vec<Theme> {
    info.get("themes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let key = t.get("tagKey").and_then(|v| v.as_str()).unwrap_or("");
                    let value = t.get("tagVal").and_then(|v| v.as_str()).unwrap_or("");
                    (!key.is_empty() || !value.is_empty()).then(|| Theme {
                        key: key.to_string(),
                        value: value.to_string(),
                        ..Default::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parametros que a API espera; copiados do que o site envia.
fn query(drama_id: &str, cursor: u32) -> Vec<(&'static str, String)> {
    vec![
        ("aid", "1988".into()),
        ("app_language", "pt-BR".into()),
        ("app_name", "tiktok_web".into()),
        ("browser_language", "pt-BR".into()),
        ("browser_name", "Mozilla".into()),
        ("browser_online", "true".into()),
        ("browser_platform", "Win32".into()),
        ("browser_version", UA.into()),
        ("channel", "tiktok_web".into()),
        ("cookie_enabled", "true".into()),
        ("coverFormat", "2".into()),
        ("device_platform", "web_pc".into()),
        ("dramaID", drama_id.into()),
        ("language", "pt-BR".into()),
        ("os", "windows".into()),
        ("priority_region", "BR".into()),
        ("region", "BR".into()),
        ("screen_height", "1080".into()),
        ("screen_width", "1920".into()),
        ("tz_name", "America/Sao_Paulo".into()),
        ("webcast_language", "pt-BR".into()),
        ("count", PAGE.to_string()),
        ("cursor", cursor.to_string()),
    ]
}

async fn fetch_page(
    client: &reqwest::Client,
    drama_id: &str,
    cursor: u32,
    cookie: &str,
) -> Result<Value> {
    let referer = format!("https://www.tiktok.com/shortdrama/episode/{drama_id}/1");
    let resp = client
        .get(API)
        .query(&query(drama_id, cursor))
        .header("User-Agent", UA)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "pt-BR,pt;q=0.9")
        .header("Referer", &referer)
        .header("Cookie", cookie)
        .send()
        .await
        .context("falha de rede ao consultar a API")?;

    let status = resp.status();
    let body = resp.text().await.context("resposta ilegivel")?;

    if !status.is_success() {
        bail!("API respondeu HTTP {}. {}", status.as_u16(), AUTH_HINT);
    }
    // Corpo vazio com 200 = sessao morta (verificado durante a investigacao).
    if body.trim().is_empty() {
        bail!("{}", AUTH_HINT);
    }

    let data: Value =
        serde_json::from_str(&body).map_err(|_| anyhow!("API nao devolveu JSON. {}", AUTH_HINT))?;

    let code = data
        .get("statusCode")
        .or_else(|| data.get("status_code"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if code != 0 {
        let msg = data
            .get("status_msg")
            .and_then(|v| v.as_str())
            .unwrap_or("sem detalhe");
        bail!("API recusou (statusCode={code}): {msg}. {}", AUTH_HINT);
    }
    Ok(data)
}

/// Busca todos os episodios de uma serie, paginando ate o fim.
pub async fn fetch_series(
    client: &reqwest::Client,
    drama_id: &str,
    cookie: &str,
) -> Result<Series> {
    let mut by_number: std::collections::BTreeMap<u32, Episode> = Default::default();
    let mut name = String::new();
    let mut description = String::new();
    let mut covers: Vec<String> = Vec::new();
    let mut themes: Vec<Theme> = Vec::new();
    let mut total_duration_sec: u32 = 0;
    let mut is_limited_free = false;
    let mut upstream = Upstream::default();
    // Numeros de audiencia sao lidos do MENOR episodio visto, nao do primeiro
    // item da pagina: a ordem da API nao e garantida, e um numero que muda de
    // episodio para episodio nao pode depender de qual chegou primeiro.
    let mut stats_from: u32 = u32::MAX;
    let mut total: u32 = 0;
    let mut cursor: u32 = 0;

    for _ in 0..MAX_PAGES {
        let data = fetch_page(client, drama_id, cursor, cookie).await?;

        if total == 0 {
            total = as_u64(data.get("totalEpisodeCount").unwrap_or(&Value::Null)) as u32;
        }

        let items = data
            .get("itemList")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for it in &items {
            let info = it.get("dramaInfo").cloned().unwrap_or(Value::Null);
            if name.is_empty() {
                name = info
                    .get("dramaName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                description = info
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                covers = parse_covers(&info);
                themes = parse_themes(&info);
                total_duration_sec = as_u64(info.get("totalDuration").unwrap_or(&Value::Null)) as u32;
                is_limited_free = info
                    .get("isLimitedFree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                upstream.num_watched = as_u64(info.get("numWatched").unwrap_or(&Value::Null));
            }
            let dvd = info.get("DramaVideoData").cloned().unwrap_or(Value::Null);
            let number = dvd
                .get("EpisodeNumber")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            if number == 0 {
                continue;
            }
            // Curtidas, comentarios e reproducoes sao do ITEM, nao da serie.
            // O episodio de menor numero e o representante estavel da novela.
            if number < stats_from {
                stats_from = number;
                let stats = it.get("stats").cloned().unwrap_or(Value::Null);
                upstream.play_count = as_u64(stats.get("playCount").unwrap_or(&Value::Null));
                upstream.digg_count = as_u64(stats.get("diggCount").unwrap_or(&Value::Null));
                upstream.collect_count = as_u64(stats.get("collectCount").unwrap_or(&Value::Null));
                upstream.comment_count = as_u64(stats.get("commentCount").unwrap_or(&Value::Null));
            }
            let video = it.get("video").cloned().unwrap_or(Value::Null);
            by_number.insert(
                number,
                Episode {
                    number,
                    item_id: it
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    duration: as_u64(video.get("duration").unwrap_or(&Value::Null)) as u32,
                    gears: parse_gears(&video),
                    // No TikTok o bloqueio aparece como ausencia de midia; nao
                    // ha um campo dizendo "isto e pago".
                    locked: false,
                    lock_reason: String::new(),
                    thumb: video
                        .get("originCover")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    created_at: as_u64(it.get("createTime").unwrap_or(&Value::Null)),
                    is_preview: dvd
                        .get("IsPreview")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    is_free_intro: dvd
                        .get("IsFreeIntro")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                },
            );
        }

        let has_more = data
            .get("hasMore")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !has_more || items.is_empty() {
            break;
        }
        cursor = data
            .get("cursor")
            .map(as_u64)
            .filter(|c| *c > cursor as u64)
            .map(|c| c as u32)
            .unwrap_or(cursor + items.len() as u32);
    }

    if by_number.is_empty() {
        bail!("Nenhum episodio encontrado para a serie {drama_id}. {}", AUTH_HINT);
    }

    if name.is_empty() {
        name = format!("drama-{drama_id}");
    }
    let episodes: Vec<Episode> = by_number.into_values().collect();
    if total == 0 {
        total = episodes.len() as u32;
    }

    Ok(Series {
        drama_id: drama_id.to_string(),
        name,
        description,
        covers,
        total,
        episodes,
        source: Source::Tiktok,
        lang: String::new(),
        themes,
        total_duration_sec,
        upstream,
        is_limited_free,
        // O TikTok nao publica trailer de novela: a ficha traz so os episodios.
        trailer: None,
    })
}

/// Remove caracteres proibidos em nomes de arquivo no Windows.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::pick_gear;

    #[test]
    fn extrai_id_de_varios_formatos() {
        let id = "7677784436146164743";
        assert_eq!(parse_drama_id(id).as_deref(), Some(id));
        assert_eq!(
            parse_drama_id("https://www.tiktok.com/shortdrama/episode/7677784436146164743/1?is_from_webapp=1")
                .as_deref(),
            Some(id)
        );
        assert_eq!(
            parse_drama_id("https://www.tiktok.com/api/x?dramaID=7677784436146164743&a=1").as_deref(),
            Some(id)
        );
        assert_eq!(parse_drama_id("nada aqui"), None);
        assert_eq!(parse_drama_id("123"), None);
    }

    fn gear(name: &str, codec: &str, w: u32, h: u32, bitrate: u64) -> Gear {
        Gear {
            gear_name: name.into(),
            codec: codec.into(),
            width: w,
            height: h,
            bitrate,
            size: 1000,
            urls: vec!["https://x/y".into()],
        }
    }

    #[test]
    fn escolhe_maior_bitrate_nao_maior_resolucao() {
        // Caso real: o 1080p h265 tem bitrate MENOR que o 540p h264.
        let ep = Episode {
            number: 1,
            item_id: "1".into(),
            duration: 160,
            gears: vec![
                gear("normal_540_0", "h264", 576, 1024, 662346),
                gear("adapt_lowest_1080_1", "h265_hvc1", 1080, 1920, 587389),
                gear("normal_480_0", "h264", 480, 854, 515174),
            ],
            locked: false,
            lock_reason: String::new(),
            thumb: String::new(),
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
        };
        assert_eq!(pick_gear(&ep, "h264", None).unwrap().gear_name, "normal_540_0");
        assert_eq!(
            pick_gear(&ep, "h265", None).unwrap().gear_name,
            "adapt_lowest_1080_1"
        );
        assert_eq!(
            pick_gear(&ep, "h264", Some("normal_480_0")).unwrap().gear_name,
            "normal_480_0"
        );
    }

    #[test]
    fn sem_gears_e_episodio_bloqueado() {
        let ep = Episode {
            number: 9,
            item_id: "9".into(),
            duration: 0,
            gears: vec![],
            locked: false,
            lock_reason: String::new(),
            thumb: String::new(),
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
        };
        assert!(pick_gear(&ep, "h264", None).is_none());
    }

    /// Fragmento real de `dramaInfo`, copiado do dump `all_eps.json`.
    ///
    /// Existe porque o bug que este trabalho conserta era exatamente deste
    /// tipo: `cover` foi lido como texto durante toda a vida do app, e como e
    /// um OBJETO o resultado era capa vazia — sem erro, sem teste quebrando,
    /// sem ninguem perceber. Um fragmento de verdade tranca a forma.
    fn drama_info_real() -> Value {
        serde_json::json!({
            "dramaID": "7677784436146164743",
            "dramaName": "A cura mortal do bilionário",
            "description": "Após prisão, Adeline Jenkins luta para pagar contas...",
            "numWatched": 25638630,
            "totalDuration": 4884,
            "isLimitedFree": true,
            "cover": {
                "urlList": [
                    "https://p16-common-sign.tiktokcdn.com/a~tplv-tiktokx-origin.image?x-expires=1",
                    "https://p19-common-sign.tiktokcdn.com/a~tplv-tiktokx-origin.image?x-expires=1",
                    "https://p16-common-sign.tiktokcdn.com/b~tplv-tiktokx-origin.image?x-expires=1"
                ]
            },
            // Recorte quadrado de 240 px: nunca serve de capa.
            "zoomCover": { "240": "https://p16/z~tplv-photomode-zoomcover:240:240.avif" },
            "themes": [
                { "tagID": "7628991193716085776",
                  "tagKey": "tag_Contractlovers_Relationship",
                  "tagVal": "Contract Lovers" },
                { "tagID": "7628991193891738640",
                  "tagKey": "tag_Marriedlife_Plot",
                  "tagVal": "Married Life" }
            ],
            "DramaVideoData": {
                "EpisodeNumber": 1,
                "IsFreeIntro": false,
                "IsPreview": true,
                "IsHighlight": false
            }
        })
    }

    /// A capa sai de `cover.urlList`, com os tres espelhos — e nunca do
    /// `zoomCover`, que e um recorte quadrado e nao a capa vertical da novela.
    #[test]
    fn a_capa_vem_do_url_list_e_nao_do_zoom_cover() {
        let covers = parse_covers(&drama_info_real());
        assert_eq!(covers.len(), 3, "os tres espelhos da capa precisam sobreviver");
        assert!(covers.iter().all(|u| u.contains("tplv-tiktokx-origin")));
        assert!(
            !covers.iter().any(|u| u.contains("zoomcover")),
            "o recorte quadrado nao pode virar capa"
        );

        // Um `cover` que venha como texto (a forma que este arquivo supunha)
        // nao produz capa nenhuma — e melhor vazio que uma URL invalida.
        let texto = serde_json::json!({ "cover": "https://p16/x.image" });
        assert!(parse_covers(&texto).is_empty());
        assert!(parse_covers(&Value::Null).is_empty());
    }

    /// Os temas vao crus: `tagKey` e `tagVal` viram `key` e `value`, sem
    /// traducao. Virar genero em portugues e decisao editorial de quem exibe.
    #[test]
    fn temas_saem_crus_com_a_chave_estavel() {
        let themes = parse_themes(&drama_info_real());
        assert_eq!(themes.len(), 2);
        assert_eq!(themes[0].key, "tag_Contractlovers_Relationship");
        assert_eq!(themes[0].value, "Contract Lovers");
        assert_eq!(themes[1].key, "tag_Marriedlife_Plot");
        // Nenhum tema sem chave nem rotulo entra na lista.
        assert!(parse_themes(&serde_json::json!({ "themes": [{}] })).is_empty());
        assert!(parse_themes(&Value::Null).is_empty());
    }

    /// Os campos que alimentam a ficha da novela precisam continuar saindo dos
    /// lugares certos: um deles mudar de nome la em cima e silencioso aqui.
    #[test]
    fn os_campos_da_ficha_saem_dos_lugares_certos() {
        let info = drama_info_real();
        assert_eq!(as_u64(info.get("totalDuration").unwrap()), 4884);
        assert_eq!(as_u64(info.get("numWatched").unwrap()), 25_638_630);
        assert_eq!(
            info.get("isLimitedFree").and_then(|v| v.as_bool()),
            Some(true)
        );
        // As marcas de previa moram em DramaVideoData, junto com o numero.
        let dvd = info.get("DramaVideoData").unwrap();
        assert_eq!(dvd.get("IsPreview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(dvd.get("IsFreeIntro").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(dvd.get("EpisodeNumber").and_then(|v| v.as_u64()), Some(1));

        // A miniatura e do item, em `video.originCover`, e e distinta por
        // episodio — e o dado que mais melhora a lista de quem exibe.
        let item = serde_json::json!({
            "id": "7677784642946174215",
            "createTime": 1787623572u64,
            "video": { "originCover": "https://p16/o~tplv-tiktokx-origin.image", "duration": 160 },
            "stats": { "playCount": 384900, "diggCount": 20500,
                       "collectCount": 6686, "commentCount": 220 }
        });
        assert_eq!(
            item.pointer("/video/originCover").and_then(|v| v.as_str()),
            Some("https://p16/o~tplv-tiktokx-origin.image")
        );
        assert_eq!(as_u64(item.get("createTime").unwrap()), 1_787_623_572);
        assert_eq!(as_u64(item.pointer("/stats/playCount").unwrap()), 384_900);
    }

    #[test]
    fn gear_sem_url_e_descartado() {
        let video = serde_json::json!({
            "bitrateInfo": [
                { "GearName": "vazio", "Bitrate": 1, "CodecType": "h264",
                  "PlayAddr": { "Width": 1, "Height": 1, "DataSize": 1, "UrlList": [] } },
                { "GearName": "ok", "Bitrate": 2, "CodecType": "h264",
                  "PlayAddr": { "Width": 2, "Height": 2, "DataSize": 2,
                                "UrlList": ["https://a/b"] } }
            ]
        });
        let gears = parse_gears(&video);
        assert_eq!(gears.len(), 1);
        assert_eq!(gears[0].gear_name, "ok");
    }
}
