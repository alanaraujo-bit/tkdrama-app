//! Fonte CandyJar (candyjar.com).
//!
//! Diferencas que moldam este modulo, todas verificadas contra o site:
//!
//! - E Next.js **App Router**, nao Pages Router como o ReelShort: nao existe
//!   `<script id="__NEXT_DATA__">`. Os dados da pagina vem em pedacos
//!   `self.__next_f.push([id,"..."])`, cada um carregando uma string JS que,
//!   uma vez decodificada, contem trechos de JSON. `next_f_text` concatena
//!   esses pedacos decodificados; `extract_json` recorta um valor especifico
//!   de dentro do resultado, contando chaves/colchetes.
//! - A pagina da serie (`/[lang]/series/<slug>-<id>`) lista os episodios
//!   (numero, bloqueio, custo) mas NAO traz a URL do video — igual ao
//!   ReelShort, e pelo mesmo motivo: so aparece na pagina do episodio, uma
//!   requisicao por episodio.
//! - So o id numerico no fim do slug importa; o resto do caminho e
//!   decorativo (o proprio site redireciona `/series/<id>` para a URL
//!   completa). Idioma default e "en" quando a URL nao traz prefixo.
//! - A midia e HLS. Nos episodios (conteudo mais novo) e master playlist com
//!   variantes h264/h265/av1 e uma faixa de audio SEPARADA (grupo `AUDIO`),
//!   em fMP4/CMAF (`init.mp4` + `.m4s`), nao MPEG-TS como o ReelShort. Em
//!   trailers (conteudo mais antigo, visto so ali) e MPEG-TS classico, video
//!   e audio ja juntos por variante — igual ao ReelShort. `select_variant`
//!   reconhece as duas formas pela presenca de `CODECS=` na playlist mestre.
//!   Por isso o download deste modulo aceita uma URL de video sempre e uma de
//!   audio OPCIONAL — quando ha as duas, o FFmpeg recebe ambas como entradas
//!   separadas e junta no mux; ele sabe buscar fragmentos fMP4 e juntar
//!   `EXT-X-MAP` sozinho, o que a leitura manual de segmentos do ReelShort
//!   nao faria com seguranca.
//!
//! Paywall: a listagem da serie ja diz `isLocked`/`isBlocked` por episodio,
//! decidido pelo SERVIDOR a partir da sessao (cookie) de quem pediu. Esse
//! calculo nunca acontece aqui: so se le o que a origem respondeu para aquela
//! sessao. E esse "cadeado" e mais fraco do que parece — sondagem ao vivo
//! (serie 725, ~60 sessoes) mostrou um teste A/B de lancamento na origem:
//! `branchData` nome A/B na pagina, por sessao. Num dos ramos o episodio da
//! FRONTIERA do bloqueio sai na propria pagina com `isLocked:false` e
//! `streamUrl` completo (playlist 200 sem cookie) — entao aquele episodio
//! "pago" tambem e resolvido quando o payload traz midia (ver
//! `resolve_media`). Os demais episodios trancados NAO levam midia no SSR:
//! a pagina so carrega o componente de paywall, e o erro honesto continua
//! sendo o de episodio pago.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;

use crate::media::{Episode, Gear, Series, Source, Trailer, UA};

const BASE: &str = "https://candyjar.com";
const DEFAULT_LANG: &str = "en";

/// Nome da unica variante que este modulo resolve (H.264 + AAC, sem transcode).
pub const GEAR_NAME: &str = "avc_aac";

pub const PAID_HINT: &str = "episódio pago no CandyJar";
pub const BLOCKED_HINT: &str = "indisponível no CandyJar";

/// Reconhece uma URL do CandyJar e extrai (idioma, id numerico da serie).
///
/// Aceita a pagina da serie (`/[lang]/series/<slug>-<id>`) e a do episodio
/// (`/[lang]/series/<slug>-<id>/episode/<n>`), com ou sem prefixo de idioma —
/// o site usa "en" quando ausente.
pub fn parse_url(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().trim_matches('"');
    if !raw.contains("candyjar.com") {
        return None;
    }
    let after_host = raw.split("candyjar.com").nth(1).unwrap_or("");
    let path = after_host.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let series_pos = segments.iter().position(|s| *s == "series")?;
    let slug = *segments.get(series_pos + 1)?;
    let id = if !slug.is_empty() && slug.chars().all(|c| c.is_ascii_digit()) {
        slug.to_string()
    } else {
        let tail = slug.rsplit('-').next()?;
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        tail.to_string()
    };

    let lang = segments
        .first()
        .filter(|s| **s != "series" && s.len() <= 5 && s.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_LANG.to_string());

    Some((lang, id))
}

fn series_url(lang: &str, id: &str) -> String {
    format!("{BASE}/{lang}/series/{id}")
}

fn episode_url(lang: &str, id: &str, number: u32) -> String {
    format!("{BASE}/{lang}/series/{id}/episode/{number}")
}

// --------------------------------------------------------------- rede

async fn get_html(client: &reqwest::Client, url: &str, cookie: &str) -> Result<String> {
    let mut req = client
        .get(url)
        .header("User-Agent", UA)
        .header("Accept-Language", "pt-BR,pt;q=0.9,en;q=0.8")
        .header("Referer", BASE);
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let resp = req.send().await.context("falha de rede ao consultar o CandyJar")?;
    let status = resp.status();
    let body = resp.text().await.context("resposta ilegivel")?;
    if !status.is_success() {
        bail!("CandyJar respondeu HTTP {}", status.as_u16());
    }
    Ok(body)
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("User-Agent", UA)
        .header("Referer", BASE)
        .send()
        .await
        .context("falha de rede ao buscar a playlist")?;
    let status = resp.status();
    let body = resp.text().await.context("playlist ilegivel")?;
    if !status.is_success() {
        bail!("playlist respondeu HTTP {}", status.as_u16());
    }
    Ok(body)
}

// ------------------------------------------------------ dados embutidos (RSC)

/// Concatena os pedacos `self.__next_f.push([id,"..."])` da pagina, decodificados.
///
/// Cada segundo elemento do array e um literal de string JS — e as regras de
/// escape dele sao um subconjunto compativel das de JSON — entao decodificar
/// basta em achar a string literal (respeitando `\"` escapado) e entregar o
/// literal inteiro (com aspas) para `serde_json::from_str::<String>`, que
/// resolve `\uXXXX`, pares substitutos etc. sem um decodificador escrito a mao.
fn next_f_text(html: &str) -> String {
    const MARKER: &str = "self.__next_f.push([";
    let bytes = html.as_bytes();
    let mut out = String::new();
    let mut idx = 0usize;

    while let Some(rel) = html[idx..].find(MARKER) {
        let mut pos = idx + rel + MARKER.len();
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b',' {
            idx = pos.max(idx + 1);
            continue;
        }
        pos += 1;
        while pos < bytes.len() && (bytes[pos] as char).is_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b'"' {
            idx = pos.max(idx + 1);
            continue;
        }
        let start = pos;
        let mut i = pos + 1;
        let mut escaped = false;
        while i < bytes.len() {
            let b = bytes[i];
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                break;
            }
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let literal = &html[start..=i];
        if let Ok(decoded) = serde_json::from_str::<String>(literal) {
            out.push_str(&decoded);
            out.push('\n');
        }
        idx = i + 1;
    }
    out
}

/// Recorta o valor JSON (objeto ou array) que segue `"key":` em `text`,
/// contando chaves/colchetes e ignorando os que aparecem dentro de strings.
///
/// Necessario porque `text` (a saida de `next_f_text`) nao e um JSON valido
/// por inteiro — e um monte de fragmentos concatenados — entao um `serde_json`
/// direto nunca funcionaria; so o trecho recortado e JSON valido.
///
/// A mesma chave pode aparecer mais de uma vez em contextos diferentes — por
/// exemplo, um dicionario de traducao tem `"episodes":"Episodes"` (rotulo de
/// menu) antes da listagem real `"episodes":[...]`. Por isso a busca
/// continua at o proximo `{`/`[` em vez de desistir na primeira ocorrencia.
fn extract_json<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    extract_json_all(text, key).into_iter().next()
}

/// Recorta TODOS os valores JSON (objeto ou array) que seguem `"key":` em
/// `text`, na ordem em que aparecem, contando chaves/colchetes e ignorando os
/// que aparecem dentro de strings.
///
/// Necessario porque `text` (a saida de `next_f_text`) nao e um JSON valido
/// por inteiro — e um monte de fragmentos concatenados — entao um
/// `serde_json` direto nunca funcionaria; so o trecho recortado e JSON valido.
///
/// A mesma chave pode aparecer mais de uma vez em contextos diferentes — por
/// exemplo, um dicionario de traducao tem `"episodes":"Episodes"` (rotulo de
/// menu) antes da listagem real `"episodes":[...]`. Por isso a busca pula a
/// ocorrencia que nao abre com `{`/`[` e continua; aqui ainda se recolhe cada
/// valor completo, porque o proprio `episodeStream` aparece duas vezes na
/// pagina de um episodio trancado (o primeiro reflete o cadeado da sessao, o
/// segundo carrega a midia completa).
fn extract_json_all<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    let pat = format!("\"{key}\":");
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while search_from < text.len() {
        let Some(rel) = text[search_from..].find(&pat) else { break };
        let idx = search_from + rel;
        let mut i = idx + pat.len();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        // Ocororrencia que nao abre um objeto/array (ex.: rotulo de
        // traducao `"episodes":"Episodes"`) — pula e continua a busca.
        let open = match bytes.get(i) {
            Some(b'{') => b'{',
            Some(b'[') => b'[',
            _ => {
                search_from = idx + pat.len();
                continue;
            }
        };
        let close = if open == b'{' { b'}' } else { b']' };
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        let mut j = i;
        let mut end: Option<usize> = None;
        while j < bytes.len() {
            let b = bytes[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_str = false;
                }
            } else if b == b'"' {
                in_str = true;
            } else if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            }
            j += 1;
        }
        match end {
            // Valor completo: recolhe e continua depois do fechamento.
            Some(j) => {
                out.push(&text[i..=j]);
                search_from = j + 1;
            }
            // Nunca fechou ate o fim do texto: nada confiavel apos ele.
            None => break,
        }
    }
    out
}

fn as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn as_bool(v: Option<&Value>) -> bool {
    v.and_then(|x| x.as_bool()).unwrap_or(false)
}

// --------------------------------------------------------------- metadados

/// Conteudo de uma tag `<meta ... content="...">`, dado o prefixo ate `content="`.
fn meta_content(html: &str, marker: &str) -> Option<String> {
    let idx = html.find(marker)?;
    let start = idx + marker.len();
    let end = html[start..].find('"')? + start;
    let raw = &html[start..end];
    if raw.trim().is_empty() {
        None
    } else {
        Some(unescape_html(raw))
    }
}

/// `<title>Watch {NOME} | ...</title>` — o nome da serie e o unico dado
/// estavel entre a versao curta (og:title) e a listagem: o resto do texto do
/// title muda de acordo com o genero anunciado.
fn title_from_html(html: &str) -> Option<String> {
    const MARK: &str = "<title>Watch ";
    let idx = html.find(MARK)?;
    let start = idx + MARK.len();
    let end = html[start..].find(" |")? + start;
    let raw = html[start..end].trim();
    if raw.is_empty() {
        None
    } else {
        Some(unescape_html(raw))
    }
}

fn unescape_html(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

// ------------------------------------------------------------------ listagem

pub async fn fetch_series(
    client: &reqwest::Client,
    lang: &str,
    id: &str,
    cookie: &str,
) -> Result<Series> {
    let html = get_html(client, &series_url(lang, id), cookie).await?;
    let blob = next_f_text(&html);

    let episodes_raw = extract_json(&blob, "episodes")
        .ok_or_else(|| anyhow!("Essa pagina do CandyJar nao lista episodios."))?;
    let items: Vec<Value> =
        serde_json::from_str(episodes_raw).context("lista de episodios do CandyJar ilegivel")?;

    // O trailer, quando existe, e o item de `episodeNumber` 0 com `isTrailer`
    // marcado. Ele resolve midia pelo MESMO caminho de um episodio comum —
    // `episode_url(lang, id, 0)` — nao precisa de tratamento especial na URL,
    // so nao entra na contagem nem na lista de episodios.
    let trailer = items
        .iter()
        .find(|it| as_u64(it.get("episodeNumber")) == 0 && as_bool(it.get("isTrailer")))
        .map(|it| Trailer {
            item_id: as_str(it.get("id")),
            thumb: {
                let vertical = as_str(it.get("verticalPosterUrl"));
                if !vertical.is_empty() {
                    vertical
                } else {
                    as_str(it.get("posterUrl"))
                }
            },
        });

    let mut episodes: Vec<Episode> = Vec::new();
    for it in &items {
        let number = as_u64(it.get("episodeNumber")) as u32;
        if number == 0 {
            continue;
        }
        let is_locked = as_bool(it.get("isLocked"));
        let is_blocked = as_bool(it.get("isBlocked"));
        let locked = is_locked || is_blocked;

        let thumb = {
            let vertical = as_str(it.get("verticalPosterUrl"));
            if !vertical.is_empty() {
                vertical
            } else {
                as_str(it.get("posterUrl"))
            }
        };

        episodes.push(Episode {
            number,
            item_id: as_str(it.get("id")),
            thumb,
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
            // A duracao real so vem depois de baixar (medida no MP4); a
            // origem nao a declara na listagem nem na pagina do episodio.
            duration: 0,
            // O cadeado da listagem nao impede a midia: o episodio trancado
            // carrega stream completo na propria pagina (paywall quebrado — ver
            // `pick_stream`). Entao todo episodio sai com um gear de reserva;
            // as URLs so existem apos baixar, quando `resolve_media` consulta a
            // pagina do episodio.
            gears: vec![Gear {
                gear_name: GEAR_NAME.into(),
                codec: "h264".into(),
                width: 0,
                height: 0,
                bitrate: 1,
                size: 0,
                urls: Vec::new(),
            }],
            locked,
            lock_reason: if !locked {
                String::new()
            } else if is_blocked {
                BLOCKED_HINT.to_string()
            } else {
                PAID_HINT.to_string()
            },
        });
    }

    if episodes.is_empty() {
        bail!("Nenhum episodio encontrado nessa novela do CandyJar.");
    }
    episodes.sort_by_key(|e| e.number);

    let name = title_from_html(&html).unwrap_or_else(|| format!("candyjar-{id}"));
    let description =
        meta_content(&html, "name=\"description\" content=\"").unwrap_or_default();
    let cover = meta_content(&html, "property=\"og:image\" content=\"");

    Ok(Series {
        trailer,
        drama_id: id.to_string(),
        name,
        description,
        covers: cover.into_iter().collect(),
        total: episodes.len() as u32,
        episodes,
        source: Source::Candyjar,
        lang: lang.to_string(),
        // O CandyJar nao publica temas, duracao total nem contadores de
        // audiencia na pagina da serie.
        themes: Vec::new(),
        total_duration_sec: 0,
        upstream: Default::default(),
        is_limited_free: false,
    })
}

// --------------------------------------------------------------------- midia

/// Escolhe qual dos objetos `episodeStream` da pagina carrega a midia do
/// episodio.
///
/// Prefere o primeiro SEM cadeado com `streamUrl` completo; na falta dele,
/// qualquer um com URL — ver `resolve_media` sobre por que, no CandyJar, isso
/// ja resolve um episodio "trancado": o paywall e a flag do payload, nao uma
/// barreira na playlist.
fn pick_stream(streams: &[Value]) -> Option<&Value> {
    let tem_url = |s: &&Value| !as_str(s.get("streamUrl")).is_empty();
    streams
        .iter()
        .find(|s| !as_bool(s.get("isLocked")) && tem_url(s))
        .or_else(|| streams.iter().find(tem_url))
}

/// Resolve a midia de um episodio: pagina do episodio -> `episodeStream` (a
/// ocorrencia que carrega midia, ver `pick_stream`) -> master playlist ->
/// (playlist de video, playlist de audio quando separada).
///
/// Devolve a URL da SUB-playlist de video (nao a master) e, quando a origem
/// entrega audio numa faixa a parte (grupo `AUDIO` do HLS — o caso comum nos
/// episodios), a URL dessa faixa tambem; nesse caso o FFmpeg recebe as duas
/// como entradas separadas e junta no mux. Conteudo mais antigo (visto em
/// trailers) chega em MPEG-TS classico com audio ja embutido na variante —
/// aqui `None`, e uma unica entrada basta.
pub async fn resolve_media(
    client: &reqwest::Client,
    lang: &str,
    id: &str,
    number: u32,
    locked: bool,
    cookie: &str,
) -> Result<(String, Option<String>)> {
    let html = get_html(client, &episode_url(lang, id, number), cookie).await?;
    let blob = next_f_text(&html);

    // A pagina pode carregar mais de um `episodeStream` — e e exatamente isso
    // que o paywall do CandyJar deixa escapar: no episodio trancado o primeiro
    // vem com `isLocked` e sem midia, mas um segundo traz a `streamUrl`
    // completa (e a playlist responde 200 mesmo sem cookie).
    let raws = extract_json_all(&blob, "episodeStream");
    if raws.is_empty() {
        bail!("episodio {number} nao existe mais");
    }
    let mut streams: Vec<Value> = Vec::with_capacity(raws.len());
    for raw in &raws {
        let v: Value =
            serde_json::from_str(raw).context("dados do episodio do CandyJar ilegiveis")?;
        streams.push(v);
    }

    // O "cadeado" e so uma flag do payload: qualquer ocorrencia com `streamUrl`
    // completo ja resolve o episodio, preferindo a que nao vem trancada. A
    // marca da listagem (`locked`) e o que se sabia ANTES desta requisicao —
    // ela nunca mais pula a pagina; serve so para decidir o motivo do erro.
    let stream = match pick_stream(&streams) {
        Some(s) => s,
        None if streams.iter().any(|s| as_bool(s.get("isLocked"))) || locked => {
            bail!("{PAID_HINT}")
        }
        None => bail!("episodio {number} sem midia acessivel"),
    };

    let stream_url = as_str(stream.get("streamUrl"));
    if stream_url.is_empty() {
        bail!("episodio {number} sem midia acessivel");
    }

    let master = get_text(client, &stream_url).await?;
    select_variant(&master, &stream_url)
}

/// Resolve uma referencia da playlist contra a URL de onde ela veio.
fn absolute(reference: &str, base: &str) -> String {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return reference.to_string();
    }
    if let Some(rest) = reference.strip_prefix('/') {
        let root = base
            .split_once("://")
            .and_then(|(scheme, rest)| {
                rest.split_once('/').map(|(host, _)| format!("{scheme}://{host}"))
            })
            .unwrap_or_else(|| base.to_string());
        return format!("{root}/{rest}");
    }
    let dir = base.split(['?', '#']).next().unwrap_or(base);
    match dir.rfind('/') {
        Some(i) => format!("{}/{}", &dir[..i], reference),
        None => reference.to_string(),
    }
}

/// Le um atributo `NOME=valor` ou `NOME="valor"` de uma linha `#EXT-X-...`,
/// exigindo que o nome comece logo apos `:` ou `,` — sem isso `BANDWIDTH=`
/// tambem acharia o meio de `AVERAGE-BANDWIDTH=`.
fn attr(line: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let bytes = line.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = line[from..].find(&pat) {
        let idx = from + rel;
        let boundary_ok = idx == 0 || matches!(bytes[idx - 1], b':' | b',');
        if boundary_ok {
            let rest = &line[idx + pat.len()..];
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    return Some(stripped[..end].to_string());
                }
            } else {
                let end = rest.find(',').unwrap_or(rest.len());
                return Some(rest[..end].to_string());
            }
        }
        from = idx + pat.len();
    }
    None
}

/// Escolhe, na playlist mestre, a melhor variante e a faixa de audio que ela
/// referencia — quando existe uma separada.
///
/// Duas formas de master ja vistas no CandyJar:
/// - **Nova** (episodios normais): variantes fMP4 com `CODECS=` declarado e
///   audio numa faixa `#EXT-X-MEDIA` a parte. Aqui se prefere H.264: nas
///   qualidades observadas ele chega com banda MAIOR que o HEVC equivalente
///   (o CandyJar parece codificar HEVC visando economia, nao qualidade), e
///   evita de quebra qualquer transcodificacao — o publicado final exige
///   H.264/AAC de qualquer forma.
/// - **Antiga** (vista em trailers, conteudo mais velho): variantes MPEG-TS
///   classicas, sem `CODECS=` e sem faixa de audio separada — video e audio
///   ja vem juntos em cada variante, igual ao ReelShort. Sem `CODECS=` para
///   filtrar, so resta escolher a maior banda; `Some("h264")`/`Some("aac")`
///   nunca chegaram a ser garantidos aqui, mas a validacao profunda do MP4
///   final recusa qualquer coisa fora disso antes de publicar.
///
/// Devolve `(video, None)` quando a variante ja carrega audio (forma antiga);
/// `(video, Some(audio))` quando sao faixas separadas (forma nova) — o
/// chamador decide, a partir disso, quantas entradas o FFmpeg recebe.
fn select_variant(master: &str, base: &str) -> Result<(String, Option<String>)> {
    let mut audio_groups: HashMap<String, String> = HashMap::new();
    for line in master.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-MEDIA:") && line.contains("TYPE=AUDIO") {
            if let (Some(group), Some(uri)) = (attr(line, "GROUP-ID"), attr(line, "URI")) {
                audio_groups.insert(group, uri);
            }
        }
    }
    // So a forma nova declara CODECS por variante. Sem nenhuma ocorrencia,
    // nao ha como nem por que filtrar por codec: e a forma antiga.
    let has_codecs = master.contains("CODECS=");

    let lines: Vec<&str> = master.lines().map(|l| l.trim()).collect();
    let mut best: Option<(u64, String, String)> = None; // (bandwidth, video_uri, audio_group)
    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        if has_codecs {
            let codecs = attr(line, "CODECS").unwrap_or_default();
            if !codecs.contains("avc1") {
                continue;
            }
        }
        let bandwidth = attr(line, "BANDWIDTH").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let audio_group = attr(line, "AUDIO").unwrap_or_default();
        let Some(uri) = lines.get(i + 1).filter(|l| !l.is_empty() && !l.starts_with('#')) else {
            continue;
        };
        if best.as_ref().map(|(b, _, _)| bandwidth > *b).unwrap_or(true) {
            best = Some((bandwidth, uri.to_string(), audio_group));
        }
    }

    let (_, video_uri, audio_group) = best.ok_or_else(|| {
        anyhow!("nenhuma variante utilizavel na playlist do CandyJar")
    })?;
    let video_url = absolute(&video_uri, base);

    let audio_url = if audio_group.is_empty() {
        None
    } else {
        let audio_uri = audio_groups
            .get(&audio_group)
            .ok_or_else(|| anyhow!("playlist do CandyJar sem faixa de audio"))?;
        Some(absolute(audio_uri, base))
    };

    Ok((video_url, audio_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconhece_url_de_serie_e_de_episodio() {
        let (lang, id) =
            parse_url("https://candyjar.com/pt/series/o-casamento-arranjado-725").unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(id, "725");

        let (lang, id) =
            parse_url("https://candyjar.com/pt/series/o-casamento-arranjado-725/episode/3")
                .unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(id, "725");
    }

    #[test]
    fn aceita_so_o_id_numerico_e_usa_idioma_default() {
        let (lang, id) = parse_url("https://candyjar.com/series/725").unwrap();
        assert_eq!(lang, "en");
        assert_eq!(id, "725");
    }

    #[test]
    fn ignora_o_que_nao_e_candyjar() {
        assert!(parse_url("https://www.reelshort.com/pt/movie/x-69d8612ad6461266e80b7a17").is_none());
        assert!(parse_url("https://candyjar.com/pt/series/sem-id").is_none());
    }

    #[test]
    fn decodifica_pedacos_do_next_f_e_recorta_por_chave() {
        let html = r#"<script>self.__next_f.push([1,"7:{\"episodes\":[{\"episodeNumber\":1,\"isLocked\":false},{\"episodeNumber\":2,\"isLocked\":true}],\"other\":1}"])</script>"#;
        let blob = next_f_text(html);
        let raw = extract_json(&blob, "episodes").expect("achou episodes");
        let items: Vec<Value> = serde_json::from_str(raw).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["episodeNumber"], 1);
        assert_eq!(items[1]["isLocked"], true);
    }

    /// Chaves/colchetes dentro de uma string (ex.: descricao com `{`) nao podem
    /// confundir a contagem de profundidade.
    #[test]
    fn extract_json_ignora_chaves_dentro_de_strings() {
        let text = r#""episode":{"id":1,"note":"contains { and [ inside"},"next":true"#;
        let raw = extract_json(text, "episode").unwrap();
        assert_eq!(raw, r#"{"id":1,"note":"contains { and [ inside"}"#);
    }

    #[test]
    fn escolhe_a_variante_h264_de_maior_banda_e_a_faixa_de_audio_certa() {
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:AUTOSELECT=YES,DEFAULT=NO,GROUP-ID=\"audio\",LANGUAGE=\"en\",TYPE=AUDIO,NAME=\"audio\",URI=\"audio/audio.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1231687,CODECS=\"hvc1.1.6.L93.90,mp4a.40.2\",RESOLUTION=1080x1920,AUDIO=\"audio\"\n\
            hevc_1080p/video.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=749863,CODECS=\"avc1.64001e,mp4a.40.2\",RESOLUTION=360x640,AUDIO=\"audio\"\n\
            360p/video.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3363145,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1080x1920,AUDIO=\"audio\"\n\
            1080p/video.m3u8\n";
        let (video, audio) =
            select_variant(master, "https://cdn.x.com/abc/playlist.m3u8?token=t").unwrap();
        assert_eq!(video, "https://cdn.x.com/abc/1080p/video.m3u8");
        assert_eq!(audio, Some("https://cdn.x.com/abc/audio/audio.m3u8".to_string()));
    }

    /// Forma antiga (vista em trailers): sem `CODECS=` e sem faixa de audio
    /// separada — so resta escolher a maior banda, e nao ha audio para juntar.
    #[test]
    fn playlist_sem_codecs_escolhe_a_maior_banda_e_nao_tem_faixa_de_audio() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=360x640\n\
            360x640/video.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1080x1920\n\
            1080x1920/video.m3u8\n";
        let (video, audio) =
            select_variant(master, "https://cdn.x.com/abc/playlist.m3u8?token=t").unwrap();
        assert_eq!(video, "https://cdn.x.com/abc/1080x1920/video.m3u8");
        assert_eq!(audio, None);
    }

    #[test]
    fn urls_relativas_da_playlist_viram_absolutas() {
        let base = "https://cdn.x.com/vod/abc/playlist.m3u8?token=x";
        assert_eq!(absolute("video1.m4s", base), "https://cdn.x.com/vod/abc/video1.m4s");
        assert_eq!(absolute("/raiz/s.m3u8", base), "https://cdn.x.com/raiz/s.m3u8");
        let abs = "https://outro.com/s.m3u8";
        assert_eq!(absolute(abs, base), abs);
    }

    /// A pagina de um episodio trancado carrega dois `episodeStream`: o
    /// primeiro reflete o cadeado da sessao (sem midia) e o segundo traz a
    /// `streamUrl` completa. A escolha tem que achar o que tem URL —
    /// preferindo o livre quando os dois existem.
    #[test]
    fn pick_stream_acha_a_midia_no_segundo_episodeStream() {
        let trancado_sem_midia = serde_json::json!({"isLocked": true});
        let completo = serde_json::json!({"streamUrl": "https://cdn.x.com/v/s.m3u8"});

        // Ordem real da pagina: o que reflete o cadeado primeiro, a midia depois.
        let streams = vec![trancado_sem_midia.clone(), completo.clone()];
        let pick = pick_stream(&streams).expect("a midia esta no segundo objeto");
        assert_eq!(pick["streamUrl"], "https://cdn.x.com/v/s.m3u8");

        // So o trancado com URL: o paywall e a flag, nao barreira — resolve.
        let so_trancado = vec![serde_json::json!({
            "isLocked": true,
            "streamUrl": "https://cdn.x.com/v/t.m3u8"
        })];
        assert_eq!(
            pick_stream(&so_trancado).unwrap()["streamUrl"],
            "https://cdn.x.com/v/t.m3u8"
        );

        // Livre com URL e trancado com URL, qualquer ordem: o livre vence.
        let ambos = vec![trancado_sem_midia, completo.clone()];
        assert_eq!(pick_stream(&ambos).unwrap()["streamUrl"], completo["streamUrl"]);

        // Sem URL em lugar nenhum: nada para escolher.
        let sem_nada = vec![serde_json::json!({"isLocked": true}), serde_json::json!({})];
        assert!(pick_stream(&sem_nada).is_none());
    }

    /// `extract_json_all` recolhe CADA ocorrencia da chave, em ordem — a base
    /// de que o episodio trancado tem dois `episodeStream` na mesma pagina.
    #[test]
    fn extract_json_all_acha_cada_ocorrencia_em_ordem() {
        let text = r#""episodeStream":{"a":1},"x":"y","episodeStream":{"b":2}"#;
        assert_eq!(
            extract_json_all(text, "episodeStream"),
            vec![r#"{"a":1}"#, r#"{"b":2}"#]
        );
        // O wrapper de uma ocorrencia continua devolvendo a primeira.
        assert_eq!(extract_json(text, "episodeStream").unwrap(), r#"{"a":1}"#);
    }

    /// Ponta a ponta contra o site de verdade. Marcado `ignore` por depender da
    /// rede. Rodar com: cargo test --lib candyjar -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn ponta_a_ponta_lista_e_resolve_episodio_livre() {
        let client = reqwest::Client::new();
        let series = fetch_series(&client, "pt", "725", "").await.expect("lista a serie");
        println!("serie: {} ({} episodios)", series.name, series.episodes.len());
        assert!(!series.name.is_empty());
        assert!(series.episodes.len() >= 40);

        let ep1 = series.episodes.iter().find(|e| e.number == 1).unwrap();
        assert!(ep1.available());
        // O corte exato de onde comeca o bloqueio varia (a origem faz teste
        // A/B de lancamento por sessao — `branchData` na pagina). O que nao
        // pode variar e o CONTRATO: trancado sai da listagem com gear de
        // reserva e motivo do bloqueio; as URLs so existem apos baixar.
        assert!(
            series.episodes.iter().any(|e| e.locked),
            "esperava pelo menos um episodio trancado/bloqueado nessa novela"
        );
        for ep in &series.episodes {
            if ep.locked {
                // Paywall quebrado no nucleo: o cadeado nao impede a midia.
                assert!(!ep.gears.is_empty(), "trancado precisa de gear de reserva");
                assert!(ep.gears[0].urls.is_empty(), "listagem nao resolve midia");
                assert!(!ep.lock_reason.is_empty());
            }
        }

        let (video, audio) = resolve_media(&client, "pt", "725", 1, false, "").await.unwrap();
        println!("video: {video}\naudio: {audio:?}");
        assert!(video.contains(".m3u8"));
        // Episodio (forma nova) tem faixa de audio separada.
        assert!(audio.is_some_and(|a| a.contains(".m3u8")));
    }

    /// A mesma listagem tem um dicionario de traducao com
    /// `"episodes":"Episodes"` (rotulo de menu) antes do array de verdade — a
    /// busca precisa pular esse falso positivo e continuar ate achar
    /// `"episodes":[...]`.
    #[test]
    fn extract_json_pula_ocorrencia_de_string_e_acha_o_array_real() {
        let text = r#""episodes":"Episodes","other":1,"episodes":[{"episodeNumber":1}]"#;
        let raw = extract_json(text, "episodes").expect("achou o array real");
        assert_eq!(raw, r#"[{"episodeNumber":1}]"#);
    }

    /// Baixa e junta video+audio de um episodio de verdade e confirma que o
    /// MP4 final passa na validacao profunda (H.264/AAC, faststart, decodifica
    /// sem erro). Marcado `ignore`: rede + FFmpeg.
    #[tokio::test]
    #[ignore]
    async fn baixa_e_remuxa_episodio_real() {
        use crate::download::{download_candyjar_mp4, validate_video, Outcome, RUN};
        use std::sync::atomic::AtomicU8;
        use std::sync::Arc;

        let client = reqwest::Client::new();
        let (video, audio) = resolve_media(&client, "pt", "725", 1, false, "").await.unwrap();

        let dir = std::env::temp_dir().join("tkdrama_cj_e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("E01.mp4");
        let control = Arc::new(AtomicU8::new(RUN));

        let outcome =
            download_candyjar_mp4(&video, audio.as_deref(), &dest, control, |_, _| {})
                .await
                .expect("baixa e junta o episodio");
        assert!(matches!(outcome, Outcome::Done(n) if n > 0));

        let info = validate_video(&dest, true).await.expect("mp4 valido");
        println!("{info:?}");
        assert_eq!(info.codec, "h264");
        assert_eq!(info.audio_codec, "aac");
        assert!(info.faststart);
        assert!(info.duration > 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serie com trailer confirmada manualmente (id=10: item `episodeNumber`
    /// 0 com `isTrailer` true). Ponta a ponta: a listagem detecta o trailer,
    /// a midia dele resolve pelo mesmo caminho de um episodio comum, e — como
    /// esse trailer especifico e conteudo antigo (MPEG-TS classico, sem faixa
    /// de audio separada) — confirma o caminho de mux com UMA entrada so.
    #[tokio::test]
    #[ignore]
    async fn detecta_baixa_e_remuxa_trailer_de_serie_real() {
        use crate::download::{download_candyjar_mp4, validate_video, Outcome, RUN};
        use std::sync::atomic::AtomicU8;
        use std::sync::Arc;

        let client = reqwest::Client::new();
        let series = fetch_series(&client, "pt", "10", "").await.expect("lista a serie");
        let trailer = series.trailer.as_ref().expect("essa serie tem trailer");
        assert!(!trailer.thumb.is_empty());

        let (video, audio) = resolve_media(&client, "pt", "10", 0, false, "")
            .await
            .expect("resolve a midia do trailer (episodio 0)");
        println!("trailer video: {video}\ntrailer audio: {audio:?}");
        assert!(video.contains(".m3u8"));

        let dir = std::env::temp_dir().join("tkdrama_cj_trailer_e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("trailer.mp4");
        let control = Arc::new(AtomicU8::new(RUN));

        let outcome =
            download_candyjar_mp4(&video, audio.as_deref(), &dest, control, |_, _| {})
                .await
                .expect("baixa e remuxa o trailer");
        assert!(matches!(outcome, Outcome::Done(n) if n > 0));

        let info = validate_video(&dest, true).await.expect("mp4 valido");
        println!("{info:?}");
        assert_eq!(info.codec, "h264");
        assert_eq!(info.audio_codec, "aac");
        assert!(info.faststart);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Em cada sessao nova, tenta resolver o PRIMEIRO, o MEIO e o ULTIMO
    /// episodio trancado via `resolve_media` — mostra se os ramos A/B
    /// liberam todos os
    /// pagos ou so parte deles. Rodar com:
    /// cargo test --lib sonde_ramos_pagos -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn sonde_ramos_pagos() {
        for i in 0..16 {
            let client = reqwest::Client::new();
            let series = fetch_series(&client, "pt", "725", "")
                .await
                .expect("lista a serie");
            let trancados: Vec<u32> = series
                .episodes
                .iter()
                .filter(|e| e.locked)
                .map(|e| e.number)
                .collect();
            if trancados.is_empty() {
                println!("[{i}] total={} sem trancados", series.episodes.len());
                continue;
            }
            let mut linha = format!(
                "[{i}] total={} trancados_{}-{}: ",
                series.episodes.len(),
                trancados.first().unwrap(),
                *trancados.last().unwrap()
            );
            let alvos = [
                *trancados.first().unwrap(),
                *trancados.get(trancados.len() / 2).unwrap_or_else(|| trancados.last().unwrap()),
                *trancados.last().unwrap(),
            ];
            for alvo in alvos {
                match resolve_media(&client, "pt", "725", alvo, true, "").await {
                    Ok(_) => linha.push_str(&format!("ep{alvo}=OK ")),
                    Err(e) => {
                        let msg = e.to_string();
                        linha.push_str(&format!("ep{alvo}=ERR({msg}) "));
                    }
                }
            }
            println!("{}", linha.trim_end());
        }
    }

    /// Sondagem do payload de um episodio trancado: despeja a pagina, o blob
    /// __next_f, todas as ocorrencias de `episodeStream` e cada `streamUrl`
    /// encontrado — para ver onde a midia real esta (ou se falta). Rodar com:
    /// cargo test --lib sonda_pagina_de_trancado -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn sonda_pagina_de_trancado() {
        let client = reqwest::Client::new();
        for number in [12u32, 40] {
            println!("===== episodio {number} =====");
            let html = get_html(&client, &episode_url("pt", "725", number), "")
                .await
                .expect("pagina do episodio");
            std::fs::write(
                std::env::temp_dir().join(format!("cj_probe_ep{number}.html")),
                &html,
            )
            .unwrap();
            let blob = next_f_text(&html);
            println!("html={} bytes, blob __next_f={} bytes", html.len(), blob.len());

            for (i, raw) in extract_json_all(&blob, "episodeStream").iter().enumerate() {
                println!("--- episodeStream[{i}] ({len} chars): {raw}", len = raw.len());
            }

            // Onde mais `streamUrl` aparece no blob?
            let mut rest = &blob[..];
            let mut achados = 0;
            while let Some(pos) = rest.find("streamUrl") {
                let fim = (pos + 350).min(rest.len());
                println!("--- streamUrl ctx: {}", &rest[pos..fim]);
                rest = &rest[fim.min(pos + 1)..]; // avança pelo menos 1
                achados += 1;
                if achados >= 6 {
                    break;
                }
            }
            println!("total de ocorrencias 'streamUrl' no blob: {achados}");
        }
    }
}

