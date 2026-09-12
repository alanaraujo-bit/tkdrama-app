//! `manifest.json`, `poster.jpg` e `thumbs/E##.jpg` na pasta da novela.
//!
//! O app ja busca nome, sinopse, capa e temas na API — eles so viviam em
//! memoria e morriam com o processo. Aqui eles viram arquivo, porque quem
//! exibe a biblioteca (o Noveleiras de Plantao) le a pasta e nao fala com o
//! TikTok.
//!
//! Tres regras carregam o modulo, todas do contrato em `CONTRATO-NOVELEIRAS.md`:
//!
//! - **URL de imagem nao serve.** As capas vem assinadas e expiram, igual as
//!   URLs de midia. Gravar o endereco produziria uma capa que vira 404 em
//!   poucos dias; por isso a imagem e baixada como arquivo.
//! - **A forma nao e livre.** Os nomes das chaves sao os que o Noveleiras ja
//!   le. `dramaID` em especial nao sobrevive ao `camelCase` automatico e
//!   precisa do `rename` explicito — sem ele o leitor do outro lado procura
//!   `dramaID`, encontra `dramaId` e cai para `null` em silencio.
//! - **Nao se inventa titulo nem sinopse de episodio.** A API nao os tem (o
//!   `desc` de todo item e a chamada da serie repetida), entao o campo fica
//!   ausente. Ausente e honesto; preenchido por chute, nao.
//!
//! Um episodio so entra no manifesto depois que o arquivo existe no disco: o
//! manifesto descreve o que ha, nao o que se pretende baixar.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::media::{Episode, Series, Source, UA};

pub const FILE: &str = "manifest.json";
const POSTER_STEM: &str = "poster";
const THUMBS_DIR: &str = "thumbs";
/// Nome fixo do trailer na pasta. E contrato com o Noveleiras, nao preferencia.
pub const TRAILER_FILE: &str = "trailer.mp4";

/// Extensoes que valem como imagem ja baixada.
const IMG_EXT: [&str; 4] = ["jpg", "png", "webp", "avif"];

// ------------------------------------------------------------------- o formato

/// Um tema declarado pela origem, cru.
///
/// Sem traducao de proposito: virar genero em portugues e decisao editorial,
/// tomada no painel do Noveleiras com alguem olhando. O `key` e o
/// identificador estavel; o `value` e o rotulo da plataforma.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Theme {
    pub key: String,
    pub value: String,
    /// Grupo da taxonomia da origem (ReelShort: `category_id`). Omitido quando
    /// a fonte nao agrupa.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
}

/// Numeros da plataforma de origem.
///
/// Isolados num bloco proprio porque o Noveleiras tem os proprios contadores
/// de audiencia, alimentados pela telemetria dele. Misturar os dois corromperia
/// a analise do painel.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    #[serde(default)]
    pub num_watched: u64,
    #[serde(default)]
    pub play_count: u64,
    #[serde(default)]
    pub digg_count: u64,
    #[serde(default)]
    pub collect_count: u64,
    #[serde(default)]
    pub comment_count: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeEntry {
    /// Nome do arquivo, como esta no disco.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Caminho relativo a pasta da novela (`thumbs/E01.jpg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Estreia na origem, ISO 8601 em UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_preview: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_free_intro: Option<bool>,
    #[serde(default)]
    pub locked: bool,
    /// Campos escritos pelo app importador ou por versões futuras sobrevivem.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl EpisodeEntry {
    /// Preenche o que o outro lado sabe, preservando o que ele nao sabe.
    ///
    /// Uma atualizacao parcial (o fim de um download, que conhece o arquivo mas
    /// pode nao ter a serie em cache) nao pode apagar a miniatura e a data que
    /// uma passagem anterior ja gravou.
    fn merge(&mut self, novo: EpisodeEntry) {
        if !novo.file.is_empty() {
            self.file = novo.file;
        }
        if novo.duration.is_some() {
            self.duration = novo.duration;
        }
        if novo.width.is_some() {
            self.width = novo.width;
        }
        if novo.height.is_some() {
            self.height = novo.height;
        }
        if novo.codec.is_some() {
            self.codec = novo.codec;
        }
        if novo.audio_codec.is_some() { self.audio_codec = novo.audio_codec; }
        if novo.container.is_some() { self.container = novo.container; }
        if novo.thumb.is_some() {
            self.thumb = novo.thumb;
        }
        if novo.item_id.is_some() {
            self.item_id = novo.item_id;
        }
        if novo.created_at.is_some() {
            self.created_at = novo.created_at;
        }
        if novo.is_preview.is_some() {
            self.is_preview = novo.is_preview;
        }
        if novo.is_free_intro.is_some() {
            self.is_free_intro = novo.is_free_intro;
        }
        self.locked = novo.locked;
    }
}

/// O manifesto inteiro.
///
/// `dramaID`, `dramaName`, `totalEpisodes` e o mapa `episodes` indexado pelo
/// numero sao o que o Noveleiras ja lia antes deste trabalho: os nomes sao
/// fixos e o resto e aditivo.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// `dramaID` e nao `dramaId`: o leitor do outro lado procura esta grafia.
    #[serde(rename = "dramaID", default)]
    pub drama_id: String,
    #[serde(default)]
    pub drama_name: String,
    #[serde(default)]
    pub total_episodes: u32,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// De onde esta novela veio.
    ///
    /// E o que permite reencontrar a serie sem o usuario ir buscar o link na
    /// plataforma de novo. Sem ela, uma pasta cuja tarefa saiu da fila vira
    /// anonima: o nome nao carrega o id, e o video so guarda o `videoID` do
    /// CDN, que nao tem caminho de volta. Guardada aqui, dentro da pasta,
    /// sobrevive a limpar a fila, reinstalar o app e trocar de maquina.
    ///
    /// Carrega tambem o idioma da pagina, que so existe aqui: no ReelShort a
    /// pagina `/en/` e a `/pt/` sao novelas em idiomas diferentes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Caminho relativo a pasta da novela. So aparece quando o arquivo existe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// `trailer.mp4`, relativo a pasta. Ausente quando a novela nao tem trailer
    /// ou quando o arquivo nao foi baixado: nunca uma URL remota, que expira.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_duration_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<Theme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Upstream>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_limited_free: Option<bool>,
    /// Quando estes metadados foram lidos da API. Serve para decidir se vale
    /// reconsultar: uma novela em andamento muda de sinopse e de contagem, uma
    /// concluida nao.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub captured_at: String,

    /// Indexado pelo NUMERO do episodio, nunca pelo nome do arquivo: o numero
    /// e a identidade estavel. O nome muda — um `.ts` do ReelShort vira `.mp4`
    /// depois da conversao do lado de quem exibe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub episodes: BTreeMap<u32, EpisodeEntry>,
    /// Preserva campos desconhecidos ao atualizar um manifesto existente.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// -------------------------------------------------------------------- tempo ISO

/// Data civil a partir de dias desde a epoca (algoritmo de Howard Hinnant).
///
/// Escrito a mao em vez de trazer uma crate de datas: sao quinze linhas contra
/// uma arvore de dependencias inteira, e o app so precisa de UTC.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix seconds -> ISO 8601 em UTC. String vazia para 0 (desconhecido).
pub fn iso_utc(epoch: u64) -> String {
    if epoch == 0 {
        return String::new();
    }
    let (y, mo, d) = civil_from_days((epoch / 86_400) as i64);
    let rem = epoch % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn now_iso() -> String {
    iso_utc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
}

// --------------------------------------------------------------- leitura/escrita

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Le o manifesto existente. Ausente ou ilegivel devolve um manifesto vazio: a
/// proxima gravacao o reconstroi, em vez de derrubar o download inteiro.
pub fn read(dir: &Path) -> Manifest {
    std::fs::read_to_string(manifest_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Grava em `.tmp` e renomeia.
///
/// Quem exibe a biblioteca pode estar varrendo a pasta neste instante, e um
/// JSON pela metade quebra a varredura inteira. No Windows o rename por cima de
/// um arquivo aberto por outro processo pode falhar (os error 32): o erro sobe,
/// e a proxima gravacao — a do episodio seguinte — refaz o trabalho.
pub fn write(dir: &Path, m: &Manifest) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(m)?;
    let tmp = dir.join("manifest.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, manifest_path(dir))?;
    Ok(())
}

// ------------------------------------------------------------------- imagens

#[cfg(test)]
fn ext_for(content_type: &str) -> &'static str {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("png") {
        "png"
    } else if ct.contains("webp") {
        "webp"
    } else if ct.contains("avif") {
        "avif"
    } else {
        "jpg"
    }
}

/// Ja existe uma imagem com este nome-base? Devolve a extensao encontrada.
fn existing_image(dir: &Path, stem: &str) -> Option<String> {
    IMG_EXT
        .iter()
        .find(|e| dir.join(format!("{stem}.{e}")).is_file())
        .map(|e| (*e).to_string())
}

/// Baixa a primeira URL que funcionar e devolve o nome do arquivo gravado.
///
/// As URLs sao espelhos da MESMA imagem (`cover.urlList` traz tres), entao
/// tentar uma apos a outra e o mesmo padrao ja usado para a midia. Se ja houver
/// arquivo, nao rebaixa: a imagem nao muda, so a assinatura da URL.
async fn fetch_image(
    client: &reqwest::Client,
    urls: &[String],
    dir: &Path,
    stem: &str,
    referer: &str,
    cookie: &str,
    headers: &[(String, String)],
    wide: bool,
) -> Result<String> {
    let final_path = dir.join(format!("{stem}.jpg"));
    if crate::download::validate_jpeg(&final_path).await.is_ok() { return Ok(format!("{stem}.jpg")); }
    // Converte uma imagem válida deixada por versão antiga (webp/png/avif).
    if let Some(ext) = existing_image(dir, stem) {
        let old = dir.join(format!("{stem}.{ext}"));
        if old != final_path && crate::download::normalize_jpeg(&old, &final_path, wide).await.is_ok() {
            return Ok(format!("{stem}.jpg"));
        }
    }
    if urls.iter().all(|u| u.trim().is_empty()) {
        bail!("sem URL de imagem");
    }
    std::fs::create_dir_all(dir)?;

    let mut last = String::new();
    for url in urls.iter().filter(|u| !u.trim().is_empty()) {
        let mut request = client.get(url)
            .header("User-Agent", UA)
            .header("Referer", referer)
            .header("Accept", "image/*,*/*");
        if !cookie.is_empty() { request = request.header("Cookie", cookie); }
        for (key, value) in headers { request = request.header(key.as_str(), value.as_str()); }
        let resp = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        if !resp.status().is_success() {
            last = format!("HTTP {}", resp.status().as_u16());
            continue;
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        // Corpo minusculo costuma ser pagina de erro do CDN, nao imagem.
        if bytes.len() < 512 {
            last = format!("resposta pequena demais ({} bytes)", bytes.len());
            continue;
        }
        // Grava e renomeia tambem aqui: uma imagem pela metade e uma capa
        // quebrada, e o outro lado so pergunta se o arquivo existe.
        let source_tmp = dir.join(format!(".{stem}.imagem.part"));
        std::fs::write(&source_tmp, &bytes)?;
        let normalized = crate::download::normalize_jpeg(&source_tmp, &final_path, wide).await;
        let _ = std::fs::remove_file(&source_tmp);
        match normalized {
            Ok(()) => return Ok(format!("{stem}.jpg")),
            Err(e) => { last = e.to_string(); continue; }
        }
    }
    bail!("nenhuma URL de imagem funcionou: {last}")
}

fn referer(source: Source) -> &'static str {
    match source {
        Source::Tiktok => "https://www.tiktok.com/",
        Source::Reelshort => "https://www.reelshort.com/",
        Source::Candyjar => "https://candyjar.com/",
        Source::Dramabox => "https://www.dramabox.com/",
    }
}

/// Baixa a capa vertical da serie. Devolve o caminho relativo a pasta.
///
/// Vem de `cover.urlList` (resolucao original) e nunca de `zoomCover`, que e um
/// recorte quadrado de 240-960 px: a capa do app e vertical, 720x1080.
pub async fn fetch_poster(
    client: &reqwest::Client, series: &Series, dir: &Path,
    cookie: &str, headers: &[(String, String)],
) -> Result<String> {
    fetch_image(
        client,
        &series.covers,
        dir,
        POSTER_STEM,
        referer(series.source),
        cookie,
        headers,
        false,
    )
    .await
}

/// Baixa a miniatura de um episodio para `thumbs/E##.<ext>`.
///
/// Devolve o caminho relativo a pasta da novela, com barra normal — e assim que
/// ele vai para o JSON e e assim que o outro lado o procura.
pub async fn fetch_thumb(
    client: &reqwest::Client,
    source: Source,
    thumb_url: &str,
    dir: &Path,
    number: u32,
    cookie: &str,
    headers: &[(String, String)],
) -> Result<String> {
    if thumb_url.trim().is_empty() {
        bail!("episodio sem miniatura na origem");
    }
    let stem = format!("E{number:02}");
    let urls = vec![thumb_url.to_string()];
    let name = fetch_image(
        client, &urls, &dir.join(THUMBS_DIR), &stem, referer(source), cookie, headers, true,
    ).await?;
    Ok(format!("{THUMBS_DIR}/{name}"))
}

fn duplicate_thumb(dir: &Path, number: u32) -> bool {
    let current = dir.join(THUMBS_DIR).join(format!("E{number:02}.jpg"));
    let Ok(bytes) = std::fs::read(&current) else { return false };
    std::fs::read_dir(dir.join(THUMBS_DIR)).ok().into_iter().flatten().flatten().any(|entry| {
        entry.path() != current
            && entry.path().extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg"))
            && std::fs::read(entry.path()).is_ok_and(|other| other == bytes)
    })
}

/// Garante miniatura local válida. Se a origem não tiver arte própria (ou
/// repetir a mesma imagem de outro episódio), extrai quadros alternativos do
/// vídeo até obter uma miniatura distinta.
pub async fn ensure_video_thumb(video: &Path, dir: &Path, number: u32) -> Result<String> {
    let thumbs = dir.join(THUMBS_DIR);
    std::fs::create_dir_all(&thumbs)?;
    let dest = thumbs.join(format!("E{number:02}.jpg"));
    if crate::download::validate_jpeg(&dest).await.is_ok() && !duplicate_thumb(dir, number) {
        return Ok(format!("{THUMBS_DIR}/E{number:02}.jpg"));
    }
    for fraction in [0.25, 0.30, 0.20, 0.35] {
        // O gerador preserva JPEG válido; removemos somente a cópia que acabou
        // de ser detectada como repetida para permitir uma nova tentativa.
        if dest.exists() { std::fs::remove_file(&dest)?; }
        crate::download::generate_thumbnail(video, &dest, fraction).await?;
        if !duplicate_thumb(dir, number) {
            return Ok(format!("{THUMBS_DIR}/E{number:02}.jpg"));
        }
    }
    bail!("não consegui gerar miniatura distinta para E{number:02}")
}

// ------------------------------------------------------------------- montagem

/// Aplica os dados de nivel de serie ao manifesto.
///
/// `poster` so entra quando o arquivo foi mesmo gravado: apontar para uma capa
/// que nao veio quebraria a imagem em vez de cair na arte gerada do outro lado.
pub fn patch_series(m: &mut Manifest, s: &Series, poster: Option<String>) {
    m.drama_id = s.drama_id.clone();
    m.drama_name = s.name.clone();
    m.total_episodes = s.total;
    m.source = match s.source {
        Source::Tiktok => "tiktok",
        Source::Reelshort => "reelshort",
        Source::Candyjar => "candyjar",
        Source::Dramabox => "dramabox",
    }
    .into();
    m.source_url = s.url();
    if !s.description.is_empty() {
        m.description = s.description.clone();
    }
    if let Some(p) = poster {
        m.poster = Some(p);
    }
    if s.total_duration_sec > 0 {
        m.total_duration_sec = Some(s.total_duration_sec as f64);
    }
    if !s.themes.is_empty() {
        m.themes = s
            .themes
            .iter()
            .map(|t| Theme {
                key: t.key.clone(),
                value: t.value.clone(),
                group: t.group.clone(),
            })
            .collect();
    }
    if s.upstream != crate::media::Upstream::default() {
        m.upstream = Some(Upstream {
            num_watched: s.upstream.num_watched,
            play_count: s.upstream.play_count,
            digg_count: s.upstream.digg_count,
            collect_count: s.upstream.collect_count,
            comment_count: s.upstream.comment_count,
        });
    }
    // So o TikTok declara a promocao gratuita. No ReelShort o campo fica
    // ausente em vez de virar `false`, que seria uma afirmacao que ninguem fez.
    if s.source == Source::Tiktok {
        m.is_limited_free = Some(s.is_limited_free);
    }
    m.captured_at = now_iso();
}

/// O que se sabe de um episodio pela API. `thumb` chega ja baixada.
pub fn episode_entry(ep: &Episode, file: String, thumb: Option<String>) -> EpisodeEntry {
    let gear = ep.gears.first();
    // As marcas de previa so existem no TikTok, e la elas vem junto com o
    // `createTime`. Sem ele, ficam ausentes em vez de `false`.
    let declara_previa = ep.created_at > 0;
    EpisodeEntry {
        file,
        duration: (ep.duration > 0).then_some(ep.duration as f64),
        width: gear.map(|g| g.width).filter(|w| *w > 0),
        height: gear.map(|g| g.height).filter(|h| *h > 0),
        codec: gear.map(|g| g.codec.clone()).filter(|c| !c.is_empty()),
        audio_codec: None,
        container: None,
        thumb,
        item_id: (!ep.item_id.is_empty()).then(|| ep.item_id.clone()),
        created_at: (ep.created_at > 0).then(|| iso_utc(ep.created_at)),
        is_preview: declara_previa.then_some(ep.is_preview),
        is_free_intro: declara_previa.then_some(ep.is_free_intro),
        locked: ep.locked,
        extra: BTreeMap::new(),
    }
}

/// Entrada final baseada no arquivo validado; os dados da API só completam
/// identidade e flags, nunca substituem o que foi medido no MP4.
pub fn validated_episode_entry(
    ep: Option<&Episode>,
    file: String,
    thumb: Option<String>,
    info: &crate::download::VideoInfo,
) -> EpisodeEntry {
    let mut entry = ep
        .map(|e| episode_entry(e, file.clone(), thumb.clone()))
        .unwrap_or_else(|| EpisodeEntry { file, thumb, ..Default::default() });
    entry.duration = Some(info.duration);
    entry.width = Some(info.width);
    entry.height = Some(info.height);
    entry.codec = Some(info.codec.clone());
    entry.audio_codec = Some(info.audio_codec.clone());
    entry.container = Some(info.container.clone());
    entry
}

/// Encaixa uma entrada de episodio, preservando o que ela nao sabe.
pub fn patch_episode(m: &mut Manifest, number: u32, entry: EpisodeEntry) {
    m.episodes.entry(number).or_default().merge(entry);
}

// -------------------------------------------------------------- pasta da novela

/// Nome do arquivo de video de um episodio, ja no disco.
///
/// O manifesto precisa apontar para o arquivo que existe, nao para o que o
/// downloader teria escrito: uma serie do ReelShort pode ter sido convertida de
/// `.ts` para `.mp4` depois. Procura pelo numero, que e a identidade estavel.
pub fn episode_file(dir: &Path, number: u32) -> Option<String> {
    episode_files(dir).remove(&number)
}

/// Todos os episodios com arquivo de video nesta pasta, numa varredura so.
///
/// Uma serie tem 40 a 90 episodios. Perguntar `episode_file` por numero leria o
/// diretorio uma vez por episodio — noventa varreduras para responder a mesma
/// pergunta. Quem precisa saber "o que ja tenho" pergunta a pasta inteira de
/// uma vez.
///
/// Preferencia por `.mp4` quando o mesmo numero aparece em mais de um formato:
/// e o arquivo que o app publica, e o `.ts` legado so sobrevive ate a
/// normalizacao passar.
pub fn episode_files(dir: &Path) -> std::collections::BTreeMap<u32, String> {
    const VIDEO: [&str; 7] = ["mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts"];
    let mut achados: std::collections::BTreeMap<u32, Vec<String>> = Default::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Default::default();
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let lower = name.to_ascii_lowercase();
        let Some((stem, ext)) = lower.rsplit_once('.') else {
            continue;
        };
        if !VIDEO.contains(&ext) {
            continue;
        }
        // Um `.parcial.mp4` abandonado por uma queda tem extensao de video e
        // numero de episodio; nunca pode virar entrada de manifesto nem fonte
        // de normalizacao — nem contar como "ja tenho".
        if is_temp_stem(stem) {
            continue;
        }
        if let Some(n) = number_in_stem(stem) {
            achados.entry(n).or_default().push(name);
        }
    }
    achados
        .into_iter()
        .map(|(n, mut nomes)| {
            nomes.sort_by_key(|name| if name.to_ascii_lowercase().ends_with(".mp4") { 0 } else { 1 });
            (n, nomes.remove(0))
        })
        .collect()
}

// ---------------------------------------------------------------------- trailer

/// O trailer ja baixado e valido, se houver.
///
/// Vale como resposta definitiva: quem tem `trailer.mp4` validado nao baixa de
/// novo. E o que torna a segunda passagem barata e sem rede.
pub async fn trailer_no_disco(dir: &Path) -> Option<String> {
    let dest = dir.join(TRAILER_FILE);
    crate::download::validate_video(&dest, false)
        .await
        .ok()
        .map(|_| TRAILER_FILE.to_string())
}

/// Baixa o trailer da origem e publica `trailer.mp4` validado.
///
/// Reaproveita inteiro o pipeline dos episodios — intermediario, validacao
/// profunda e rename atomico — porque o contrato do trailer e o mesmo do
/// episodio: MP4 real, H.264/AAC, faststart. Devolve `None` quando a novela
/// simplesmente nao tem trailer, que nao e erro.
///
/// `cj_cookie` so importa para o CandyJar (sessao usada ao resolver a midia);
/// as outras fontes ignoram o parametro.
pub async fn ensure_trailer(
    client: &reqwest::Client,
    series: &Series,
    dir: &Path,
    sess: &crate::reelshort::Session,
    cj_cookie: &str,
    control: crate::download::Control,
) -> Result<Option<String>> {
    if let Some(pronto) = trailer_no_disco(dir).await {
        return Ok(Some(pronto));
    }
    let Some(trailer) = &series.trailer else {
        return Ok(None);
    };
    let dest = dir.join(TRAILER_FILE);

    // `serial_number`/`episodeNumber` 0 e o proprio numero do trailer na
    // origem; a URL do episodio se monta com ele. `locked: false`: trailer e
    // sempre aberto (nunca fica atras do paywall).
    let outcome = match series.source {
        Source::Reelshort => {
            let (segments, _) = crate::reelshort::resolve_media(
                client, &series.lang, &series.drama_id, 0, &trailer.item_id, false, sess,
            )
            .await?;
            crate::download::download_hls_mp4(client, &segments, &dest, "", control, |_, _| {})
                .await?
        }
        Source::Candyjar => {
            let (video, audio) = crate::candyjar::resolve_media(
                client, &series.lang, &series.drama_id, 0, false, cj_cookie,
            )
            .await?;
            crate::download::download_candyjar_mp4(
                &video, audio.as_deref(), &dest, control, |_, _| {},
            )
            .await?
        }
        // Nenhuma das duas publica video de apresentacao: `series.trailer` ja
        // vem `None`, e chegar aqui so aconteceria com um manifesto inventado.
        Source::Tiktok | Source::Dramabox => return Ok(None),
    };
    match outcome {
        crate::download::Outcome::Done(_) => {
            crate::download::validate_video(&dest, true).await?;
            Ok(Some(TRAILER_FILE.to_string()))
        }
        // Pausa ou cancelamento preservam o intermediario e nao mentem no
        // manifesto: sem arquivo publicado, sem campo.
        _ => Ok(None),
    }
}

/// Converte/normaliza o arquivo local e devolve nome + metadados medidos.
/// Serve tanto ao backfill quanto à sincronização após download.
pub async fn ensure_episode_video(
    dir: &Path,
    series_name: &str,
    number: u32,
) -> Result<(String, crate::download::VideoInfo)> {
    let final_name = format!("{} - E{number:02}.mp4", crate::media::safe_name(series_name));
    let dest = dir.join(&final_name);
    if let Ok(info) = crate::download::validate_video(&dest, false).await {
        return Ok((final_name, info));
    }
    let found = episode_file(dir, number).ok_or_else(|| anyhow::anyhow!("episódio E{number:02} ausente"))?;
    let input = dir.join(found);
    let control = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::download::RUN));
    match crate::download::prepare_video(&input, &dest, &control).await? {
        crate::download::Outcome::Done(_) => {
            let info = crate::download::validate_video(&dest, false).await?;
            Ok((final_name, info))
        }
        _ => bail!("normalização interrompida"),
    }
}

/// Marca os intermediarios que o downloader escreve ao lado do arquivo final.
/// Recebe o stem ja em minusculas.
fn is_temp_stem(stem: &str) -> bool {
    stem.ends_with(".parcial") || stem.ends_with(".part") || stem.ends_with(".tmp")
}

/// Le o `E##` do nome do arquivo. Aceita `- E01` e `E1`, que e o que o app
/// escreve e o que sobrevive a um renomear a mao.
fn number_in_stem(stem: &str) -> Option<u32> {
    let chars: Vec<char> = stem.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != 'e' {
            continue;
        }
        // Precisa estar isolado: um "e" no meio de uma palavra nao marca nada.
        // Digito antes passa por causa de `S01E12`, que e forma corrente e e
        // uma das que quem exibe a biblioteca tambem reconhece.
        if i > 0 && !matches!(chars[i - 1], ' ' | '-' | '_' | '.' | '0'..='9') {
            continue;
        }
        let digits: String = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() || digits.len() > 4 {
            continue;
        }
        // Fim do nome ou fronteira: "E01" sim, "E01x" nao.
        if chars
            .get(i + 1 + digits.len())
            .is_some_and(|c| c.is_alphanumeric())
        {
            continue;
        }
        match digits.parse::<u32>() {
            Ok(n) if n > 0 => return Some(n),
            _ => continue,
        }
    }
    None
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LocalBackfillReport {
    pub converted: usize,
    pub episodes: usize,
    pub thumbs: usize,
    pub errors: Vec<String>,
}

/// Repara uma pasta usando exclusivamente os arquivos locais. Não consulta a
/// origem e, portanto, funciona para bibliotecas antigas e sessões expiradas.
pub async fn backfill_local(dir: &Path) -> Result<LocalBackfillReport> {
    let mut report = LocalBackfillReport::default();
    let mut manifest = read(dir);
    let series_name = if manifest.drama_name.trim().is_empty() {
        dir.file_name().unwrap_or_default().to_string_lossy().to_string()
    } else { manifest.drama_name.clone() };

    // O trailer entra no manifesto so quando ha arquivo validado no disco. Um
    // `trailer` declarado por uma passagem anterior cujo arquivo sumiu tem de
    // desaparecer daqui, senao o outro lado publica um botao que da 404.
    manifest.trailer = trailer_no_disco(dir).await;

    // Poster legado passa a cumprir o contrato fixo `poster.jpg`.
    if crate::download::validate_jpeg(&dir.join("poster.jpg")).await.is_ok() {
        manifest.poster = Some("poster.jpg".into());
    } else if let Some(ext) = existing_image(dir, POSTER_STEM) {
        let old = dir.join(format!("poster.{ext}"));
        if old != dir.join("poster.jpg")
            && crate::download::normalize_jpeg(&old, &dir.join("poster.jpg"), false).await.is_ok()
        {
            manifest.poster = Some("poster.jpg".into());
        }
    }

    let mut numbers = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let Some((stem, ext)) = name.rsplit_once('.') else { continue };
        if ["mp4", "ts", "m2ts", "mkv", "mov", "m4v", "webm"].contains(&ext) && !is_temp_stem(stem) {
            if let Some(number) = number_in_stem(stem) { numbers.push(number); }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();

    let old_entries = manifest.episodes.clone();
    manifest.episodes.clear();
    for number in numbers {
        let had_ts = dir.join(format!("{} - E{number:02}.ts", crate::media::safe_name(&series_name))).exists()
            || episode_file(dir, number).is_some_and(|f| f.to_ascii_lowercase().ends_with(".ts"));
        match ensure_episode_video(dir, &series_name, number).await {
            Ok((file, info)) => {
                if had_ts { report.converted += 1; }
                let video = dir.join(&file);
                match ensure_video_thumb(&video, dir, number).await {
                    Ok(thumb) => {
                        report.thumbs += 1;
                        let mut entry = old_entries.get(&number).cloned().unwrap_or_default();
                        entry.merge(validated_episode_entry(None, file, Some(thumb), &info));
                        manifest.episodes.insert(number, entry);
                        report.episodes += 1;
                    }
                    Err(e) => report.errors.push(format!("E{number:02} thumbnail: {e}")),
                }
            }
            Err(e) => report.errors.push(format!("E{number:02} vídeo: {e}")),
        }
    }
    if manifest.total_episodes == 0 { manifest.total_episodes = manifest.episodes.len() as u32; }
    let total: f64 = manifest.episodes.values().filter_map(|e| e.duration).sum();
    if total > 0.0 { manifest.total_duration_sec = Some(total); }
    if manifest.drama_name.is_empty() { manifest.drama_name = series_name; }
    if manifest.captured_at.is_empty() { manifest.captured_at = now_iso(); }
    write(dir, &manifest)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Gear, Theme as MediaTheme, Upstream as MediaUpstream};

    fn dir_de_teste(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tkdrama_manifest_{nome}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn serie() -> Series {
        Series {
            drama_id: "7677784436146164743".into(),
            name: "A cura mortal do bilionario".into(),
            description: "Apos prisao, Adeline Jenkins luta para pagar contas...".into(),
            covers: vec!["https://p16/cover.image?x-signature=a".into()],
            total: 40,
            episodes: vec![Episode {
                number: 1,
                item_id: "7677784642946174215".into(),
                duration: 160,
                gears: vec![Gear {
                    gear_name: "normal_540_0".into(),
                    codec: "h264".into(),
                    width: 576,
                    height: 1024,
                    bitrate: 662_346,
                    size: 10,
                    urls: vec!["https://x".into()],
                }],
                locked: false,
                lock_reason: String::new(),
                thumb: "https://p16/origin.image".into(),
                created_at: 1_787_623_572,
                is_preview: true,
                is_free_intro: false,
            }],
            source: Source::Tiktok,
            lang: String::new(),
            themes: vec![MediaTheme {
                key: "tag_Contractlovers_Relationship".into(),
                value: "Contract Lovers".into(),
                ..Default::default()
            }],
            total_duration_sec: 4884,
            upstream: MediaUpstream {
                num_watched: 25_638_630,
                play_count: 384_900,
                digg_count: 20_500,
                collect_count: 6_686,
                comment_count: 220,
            },
            is_limited_free: true,
            trailer: None,
        }
    }

    /// Contrato com quem exibe a biblioteca. As chaves sao lidas do outro lado
    /// por string literal: renomear um campo aqui nao quebra compilacao
    /// nenhuma, so faz a capa ou a sinopse sumirem em silencio.
    #[test]
    fn as_chaves_sao_exatamente_as_que_o_outro_lado_le() {
        let s = serie();
        let mut m = Manifest::default();
        patch_series(&mut m, &s, Some("poster.jpg".into()));
        patch_episode(
            &mut m,
            1,
            episode_entry(
                &s.episodes[0],
                "A cura mortal do bilionario - E01.mp4".into(),
                Some("thumbs/E01.jpg".into()),
            ),
        );

        let v = serde_json::to_value(&m).unwrap();
        let obj = v.as_object().unwrap();
        for chave in [
            "dramaID",
            "dramaName",
            "totalEpisodes",
            "source",
            "sourceUrl",
            "description",
            "poster",
            "totalDurationSec",
            "themes",
            "upstream",
            "isLimitedFree",
            "capturedAt",
            "episodes",
        ] {
            assert!(obj.contains_key(chave), "chave `{chave}` sumiu do manifesto");
        }
        // `dramaID` nao pode virar `dramaId`: e assim que ele e procurado.
        assert!(!obj.contains_key("dramaId"));
        assert_eq!(obj["dramaID"], serde_json::json!("7677784436146164743"));
        assert_eq!(obj["source"], serde_json::json!("tiktok"));
        assert_eq!(obj["totalDurationSec"], serde_json::json!(4884.0));
        assert_eq!(
            obj["themes"][0]["key"],
            serde_json::json!("tag_Contractlovers_Relationship")
        );
        assert_eq!(obj["themes"][0]["value"], serde_json::json!("Contract Lovers"));
        assert_eq!(obj["upstream"]["numWatched"], serde_json::json!(25_638_630u64));
        assert_eq!(obj["upstream"]["playCount"], serde_json::json!(384_900u64));
        assert_eq!(obj["isLimitedFree"], serde_json::json!(true));

        // O mapa de episodios e indexado pelo numero, em texto.
        let e1 = &obj["episodes"]["1"];
        for chave in [
            "file",
            "duration",
            "width",
            "height",
            "codec",
            "thumb",
            "itemId",
            "createdAt",
            "isPreview",
            "isFreeIntro",
            "locked",
        ] {
            assert!(e1.get(chave).is_some(), "chave `{chave}` sumiu do episodio");
        }
        assert_eq!(e1["createdAt"], serde_json::json!("2026-08-25T02:06:12Z"));
        assert_eq!(e1["thumb"], serde_json::json!("thumbs/E01.jpg"));
        // `itemId` e o id do ITEM, nao o da serie.
        assert_eq!(e1["itemId"], serde_json::json!("7677784642946174215"));
        assert_eq!(e1["isPreview"], serde_json::json!(true));
        assert_eq!(e1["codec"], serde_json::json!("h264"));
    }

    /// Titulo e sinopse por episodio nao existem na origem — o `desc` de todo
    /// item e a chamada da serie repetida. Um resumo adivinhado engana quem le
    /// o catalogo, entao o campo simplesmente nao pode existir.
    #[test]
    fn nunca_inventa_titulo_nem_sinopse_de_episodio() {
        let s = serie();
        let mut m = Manifest::default();
        patch_episode(&mut m, 1, episode_entry(&s.episodes[0], "E01.mp4".into(), None));
        let v = serde_json::to_value(&m).unwrap();
        let e1 = v["episodes"]["1"].as_object().unwrap();
        assert!(!e1.contains_key("title"));
        assert!(!e1.contains_key("synopsis"));
        assert!(!e1.contains_key("desc"));
        // Sem miniatura baixada o campo fica AUSENTE, nao vazio: o outro lado
        // cai na arte gerada, que e melhor que um caminho quebrado.
        assert!(!e1.contains_key("thumb"));
    }

    /// No ReelShort nao ha miniatura de capitulo nem marca de previa. Faltar e
    /// melhor que inventar: o outro lado cai na arte gerada.
    #[test]
    fn reelshort_sai_sem_miniatura_e_sem_marca_de_previa() {
        let ep = Episode {
            number: 3,
            item_id: "10mdp166yi".into(),
            duration: 90,
            gears: vec![],
            locked: false,
            lock_reason: String::new(),
            thumb: String::new(),
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
        };
        let e = episode_entry(&ep, "Novela - E03.ts".into(), None);
        assert!(e.thumb.is_none());
        assert!(e.created_at.is_none());
        assert!(e.is_preview.is_none());
        assert!(e.is_free_intro.is_none());
        assert_eq!(e.duration, Some(90.0));
    }

    /// Uma fila interrompida no episodio 2 tem que deixar um manifesto com
    /// dois episodios, nao nenhum — e o do 1 nao pode perder o que ja sabia.
    #[test]
    fn atualizacao_incremental_preserva_o_que_ja_havia() {
        let dir = dir_de_teste("incremental");
        let s = serie();

        let mut m = read(&dir);
        patch_series(&mut m, &s, Some("poster.jpg".into()));
        patch_episode(
            &mut m,
            1,
            episode_entry(&s.episodes[0], "E01.mp4".into(), Some("thumbs/E01.jpg".into())),
        );
        write(&dir, &m).unwrap();

        // Segunda passagem sem a serie em cache: so o que o download sabe.
        let mut m = read(&dir);
        assert_eq!(m.episodes.len(), 1);
        patch_episode(
            &mut m,
            2,
            EpisodeEntry {
                file: "E02.mp4".into(),
                duration: Some(120.0),
                ..Default::default()
            },
        );
        // O episodio 1 recebe so o nome do arquivo; a miniatura tem que ficar.
        patch_episode(
            &mut m,
            1,
            EpisodeEntry {
                file: "E01.mp4".into(),
                ..Default::default()
            },
        );
        write(&dir, &m).unwrap();

        let lido = read(&dir);
        assert_eq!(lido.episodes.len(), 2);
        assert_eq!(lido.episodes[&1].thumb.as_deref(), Some("thumbs/E01.jpg"));
        assert_eq!(lido.episodes[&1].duration, Some(160.0));
        assert_eq!(lido.episodes[&2].duration, Some(120.0));
        // Os dados de serie sobrevivem a uma atualizacao so de episodio.
        assert_eq!(lido.drama_name, "A cura mortal do bilionario");
        assert_eq!(lido.total_episodes, 40);
        assert_eq!(lido.poster.as_deref(), Some("poster.jpg"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gravacao_e_atomica_e_nao_deixa_tmp() {
        let dir = dir_de_teste("atomica");
        let m = Manifest {
            drama_id: "1".into(),
            ..Default::default()
        };
        write(&dir, &m).unwrap();
        assert!(dir.join(FILE).is_file());
        assert!(!dir.join("manifest.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifesto_ilegivel_nao_derruba_a_gravacao() {
        let dir = dir_de_teste("ilegivel");
        std::fs::write(dir.join(FILE), "{ isto nao e json").unwrap();
        // Le como vazio e reconstroi, em vez de abortar o download inteiro.
        let m = read(&dir);
        assert!(m.drama_id.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_iso_bate_com_o_dump_real() {
        // Episodio 1 de "A cura mortal do bilionario", createTime do dump.
        assert_eq!(iso_utc(1_787_623_572), "2026-08-25T02:06:12Z");
        assert_eq!(iso_utc(0), "");
        assert_eq!(iso_utc(946_684_800), "2000-01-01T00:00:00Z");
        // 29 de fevereiro existe: um ano bissexto mal contado erraria a data.
        assert_eq!(iso_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    /// O arquivo casa pelo NUMERO, nao pela extensao: um `.ts` do ReelShort
    /// convertido para `.mp4` do outro lado continua sendo o mesmo episodio.
    #[test]
    fn encontra_o_arquivo_do_episodio_no_disco() {
        let dir = dir_de_teste("arquivo");
        std::fs::write(dir.join("Novela X - E01.mp4"), b"v").unwrap();
        std::fs::write(dir.join("Novela X - E02.ts"), b"v").unwrap();
        std::fs::write(dir.join("Novela X - E03.mp4.part"), b"v").unwrap();
        std::fs::write(dir.join("poster.jpg"), b"i").unwrap();

        assert_eq!(episode_file(&dir, 1).as_deref(), Some("Novela X - E01.mp4"));
        assert_eq!(episode_file(&dir, 2).as_deref(), Some("Novela X - E02.ts"));
        // Download em curso nao e episodio.
        assert_eq!(episode_file(&dir, 3), None);
        assert_eq!(episode_file(&dir, 9), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A varredura de pasta inteira responde o mesmo que perguntar um a um.
    ///
    /// E ela que sustenta o "nunca baixa o mesmo episodio duas vezes": quem
    /// enfileira consulta a pasta por NUMERO, e nao pelo nome que o downloader
    /// escreveria hoje. Um arquivo renomeado, ou vindo de quando a novela tinha
    /// outro nome, continua contando como "ja tenho".
    #[test]
    fn a_varredura_da_pasta_acha_o_episodio_mesmo_renomeado() {
        let dir = dir_de_teste("varredura");
        std::fs::write(dir.join("Novela X - E01.mp4"), b"v").unwrap();
        // Nome antigo da mesma novela: o numero e a identidade estavel.
        std::fs::write(dir.join("Titulo Antigo - E02.mp4"), b"v").unwrap();
        // Legado ainda nao normalizado.
        std::fs::write(dir.join("Novela X - E03.ts"), b"v").unwrap();
        // Interrompido: nao conta.
        std::fs::write(dir.join("Novela X - E04.parcial.mp4"), b"v").unwrap();
        std::fs::write(dir.join("poster.jpg"), b"i").unwrap();

        let achados = episode_files(&dir);
        assert_eq!(achados.get(&1).map(String::as_str), Some("Novela X - E01.mp4"));
        assert_eq!(achados.get(&2).map(String::as_str), Some("Titulo Antigo - E02.mp4"));
        assert_eq!(achados.get(&3).map(String::as_str), Some("Novela X - E03.ts"));
        assert_eq!(achados.get(&4), None);
        assert_eq!(achados.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mesmo numero em dois formatos: vence o MP4, que e o que o app publica.
    #[test]
    fn entre_mp4_e_ts_do_mesmo_episodio_vence_o_mp4() {
        let dir = dir_de_teste("varredura-dupla");
        std::fs::write(dir.join("Novela X - E07.ts"), b"v").unwrap();
        std::fs::write(dir.join("Novela X - E07.mp4"), b"v").unwrap();

        assert_eq!(
            episode_files(&dir).get(&7).map(String::as_str),
            Some("Novela X - E07.mp4")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pasta que nao existe nao e erro: e uma novela que nunca foi baixada.
    #[test]
    fn pasta_inexistente_devolve_vazio_sem_estourar() {
        let dir = std::env::temp_dir().join("tkdrama-nao-existe-mesmo");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(episode_files(&dir).is_empty());
    }

    /// Trailer so conta quando e MP4 valido no disco. Um arquivo com o nome
    /// certo mas conteudo quebrado nao vira campo no manifesto.
    #[tokio::test]
    async fn trailer_invalido_no_disco_nao_vira_campo() {
        let dir = dir_de_teste("trailer-invalido");
        assert_eq!(trailer_no_disco(&dir).await, None, "sem arquivo, sem trailer");

        std::fs::write(dir.join(TRAILER_FILE), b"nao sou um mp4").unwrap();
        assert_eq!(trailer_no_disco(&dir).await, None, "lixo com nome de mp4 nao vale");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Trailer declarado num manifesto antigo cujo arquivo sumiu tem de sair do
    /// manifesto: senao o Noveleiras publica um botao que da 404.
    #[tokio::test]
    async fn backfill_apaga_trailer_declarado_que_sumiu() {
        let dir = dir_de_teste("trailer-fantasma");
        let mut m = Manifest { drama_name: "Novela X".into(), ..Default::default() };
        m.trailer = Some(TRAILER_FILE.into());
        write(&dir, &m).unwrap();
        assert!(read(&dir).trailer.is_some(), "o manifesto de partida declara trailer");

        backfill_local(&dir).await.unwrap();
        assert_eq!(read(&dir).trailer, None, "sem arquivo no disco, o campo cai");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O remux interrompido deixa um `.parcial.mp4`: extensao de video e numero
    /// de episodio validos. Ele nao pode entrar no manifesto nem ser escolhido
    /// como fonte no lugar do `.ts` intacto.
    #[test]
    fn intermediario_nao_conta_como_episodio() {
        let dir = dir_de_teste("parcial");
        std::fs::write(dir.join("Novela X - E01.parcial.mp4"), b"v").unwrap();
        std::fs::write(dir.join("Novela X - E02.parcial.mp4"), b"v").unwrap();
        std::fs::write(dir.join("Novela X - E02.ts"), b"v").unwrap();

        assert_eq!(episode_file(&dir, 1), None);
        assert_eq!(episode_file(&dir, 2).as_deref(), Some("Novela X - E02.ts"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn le_o_numero_so_quando_ele_marca_episodio() {
        assert_eq!(number_in_stem("novela - e01"), Some(1));
        assert_eq!(number_in_stem("e7"), Some(7));
        assert_eq!(number_in_stem("s01e12"), Some(12));
        // "e" grudado numa palavra nao e marca de episodio.
        assert_eq!(number_in_stem("bilionare01x"), None);
        assert_eq!(number_in_stem("sem numero"), None);
        assert_eq!(number_in_stem("e00"), None);
    }

    #[test]
    fn imagem_ja_baixada_nao_e_rebaixada() {
        let dir = dir_de_teste("imagem");
        assert!(existing_image(&dir, POSTER_STEM).is_none());
        std::fs::write(dir.join("poster.webp"), b"i").unwrap();
        assert_eq!(existing_image(&dir, POSTER_STEM).as_deref(), Some("webp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extensao_vem_do_tipo_de_conteudo() {
        assert_eq!(ext_for("image/webp"), "webp");
        assert_eq!(ext_for("image/png"), "png");
        assert_eq!(ext_for("image/jpeg"), "jpg");
        // Sem tipo declarado, jpg — que e o nome que o contrato usa.
        assert_eq!(ext_for(""), "jpg");
    }

    /// Ponta a ponta contra o site de verdade: capa da serie, trailer e
    /// miniatura de episodio, todos do CandyJar — o mesmo padrao que TikTok e
    /// ReelShort ja tinham. Serie id=10 confirmada com trailer manualmente.
    /// Marcado `ignore`: rede + FFmpeg.
    #[tokio::test]
    #[ignore]
    async fn candyjar_grava_capa_trailer_e_miniatura_como_as_outras_fontes() {
        let client = reqwest::Client::new();
        let series = crate::candyjar::fetch_series(&client, "pt", "10", "")
            .await
            .expect("lista a serie");
        assert!(series.trailer.is_some(), "essa serie tem trailer conhecido");
        assert!(!series.covers.is_empty(), "essa serie tem capa (og:image)");

        let dir = dir_de_teste("candyjar_e2e");

        let poster = fetch_poster(&client, &series, &dir, "", &[]).await.expect("baixa a capa");
        assert_eq!(poster, "poster.jpg");
        assert!(dir.join("poster.jpg").is_file());

        let control = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::download::RUN));
        let trailer = ensure_trailer(&client, &series, &dir, &crate::reelshort::Session::default(), "", control)
            .await
            .expect("baixa o trailer")
            .expect("essa serie tem trailer");
        assert_eq!(trailer, TRAILER_FILE);
        assert!(dir.join(TRAILER_FILE).is_file());

        let ep1 = series.episodes.iter().find(|e| e.number == 1).expect("episodio 1 existe");
        let thumb = fetch_thumb(&client, series.source, &ep1.thumb, &dir, 1, "", &[])
            .await
            .expect("baixa a miniatura do episodio 1");
        assert_eq!(thumb, format!("{THUMBS_DIR}/E01.jpg"));
        assert!(dir.join(&thumb).is_file());

        // Monta e grava o manifesto de verdade — o formato que o Noveleiras le.
        let mut m = read(&dir);
        patch_series(&mut m, &series, Some(poster));
        m.trailer = Some(trailer);
        let entry = episode_entry(ep1, "placeholder - E01.mp4".into(), Some(thumb));
        patch_episode(&mut m, 1, entry);
        write(&dir, &m).expect("grava o manifesto");

        let raw = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        println!("{raw}");

        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["source"], "candyjar");
        assert_eq!(v["sourceUrl"], series.url());
        // O trailer (episodeNumber 0) nao pode contar como episodio: o total
        // gravado precisa bater exatamente com o que a listagem devolveu.
        assert_eq!(v["totalEpisodes"], series.episodes.len() as u64);
        assert_eq!(v["poster"], "poster.jpg");
        assert_eq!(v["trailer"], "trailer.mp4");
        assert_eq!(v["episodes"]["1"]["thumb"], format!("{THUMBS_DIR}/E01.jpg"));
        assert_eq!(v["episodes"]["1"]["locked"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ponta a ponta contra o site de verdade: capa, miniatura, download real
    /// do episodio 1 e manifesto final — o DramaBox no mesmo formato que as
    /// outras fontes. Diferente do CandyJar, aqui o video e MP4 progressivo e
    /// desce pelo `download_episode`, o caminho do TikTok; e o unico jeito de
    /// provar que a fonte nova foi ligada no downloader certo.
    /// Marcado `ignore`: rede + FFmpeg.
    #[tokio::test]
    #[ignore]
    async fn dramabox_grava_capa_miniatura_e_baixa_o_episodio_de_verdade() {
        let client = reqwest::Client::new();
        let series = crate::dramabox::fetch_series(&client, "pt", "42000000639", "")
            .await
            .expect("lista a serie");
        assert!(!series.covers.is_empty(), "essa serie tem capa");
        assert!(series.trailer.is_none(), "o DramaBox nao publica trailer");

        let dir = dir_de_teste("dramabox_e2e");

        let poster = fetch_poster(&client, &series, &dir, "", &[])
            .await
            .expect("baixa a capa");
        assert_eq!(poster, "poster.jpg");
        assert!(dir.join("poster.jpg").is_file());

        let ep1 = series
            .episodes
            .iter()
            .find(|e| e.number == 1)
            .expect("episodio 1 existe");
        let thumb = fetch_thumb(&client, series.source, &ep1.thumb, &dir, 1, "", &[])
            .await
            .expect("baixa a miniatura do episodio 1");
        assert_eq!(thumb, format!("{THUMBS_DIR}/E01.jpg"));
        assert!(dir.join(&thumb).is_file());

        // Sem trailer: a funcao precisa dizer "nao tem" em vez de falhar.
        let control = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::download::RUN));
        let sem_trailer = ensure_trailer(
            &client,
            &series,
            &dir,
            &crate::reelshort::Session::default(),
            "",
            control.clone(),
        )
        .await
        .expect("nao e erro nao ter trailer");
        assert!(sem_trailer.is_none());

        // O download de verdade, pelo caminho progressivo. `gear.size` vem 0
        // (a origem nao declara tamanho): o downloader tem que se virar com o
        // Content-Length da resposta.
        let gear = ep1.gears.first().expect("episodio 1 liberado");
        assert_eq!(gear.size, 0, "a listagem do DramaBox nao declara tamanho");
        let dest = dir.join("ep01.mp4");
        let outcome =
            crate::download::download_episode(&client, gear, &dest, "", control, |_, _| {})
                .await
                .expect("baixa o episodio 1");
        assert!(matches!(outcome, crate::download::Outcome::Done(_)));

        // MP4 real, publicavel — o contrato entregue a quem exibe a biblioteca.
        let info = crate::download::validate_video(&dest, true)
            .await
            .expect("o episodio baixado e um MP4 valido");
        assert!(info.width > 0 && info.height > 0, "video sem dimensao");
        assert!(
            info.height > info.width,
            "novela vertical: {}x{}",
            info.width,
            info.height
        );

        let mut m = read(&dir);
        patch_series(&mut m, &series, Some(poster));
        let entry = episode_entry(ep1, "ep01.mp4".into(), Some(thumb));
        patch_episode(&mut m, 1, entry);
        write(&dir, &m).expect("grava o manifesto");

        let raw = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["source"], "dramabox");
        assert_eq!(v["sourceUrl"], series.url());
        assert_eq!(v["poster"], "poster.jpg");
        // Sem trailer, o campo simplesmente nao existe — nunca `null`.
        assert!(v.get("trailer").is_none() || v["trailer"].is_null());
        assert_eq!(v["episodes"]["1"]["thumb"], format!("{THUMBS_DIR}/E01.jpg"));
        assert_eq!(v["episodes"]["1"]["locked"], false);
        // Duracao em segundos, nao nos milissegundos que a origem declara.
        let dur = v["episodes"]["1"]["duration"].as_f64().unwrap();
        assert!(dur > 30.0 && dur < 3600.0, "duracao fora de escala: {dur}");
        // E a mesma duracao que o MP4 baixado mede: prova que a conversao de
        // milissegundos para segundos foi feita, e na escala certa.
        assert!(
            (dur - info.duration).abs() < 5.0,
            "duracao declarada {dur}s longe da medida {}s",
            info.duration
        );
        // Temas crus, como a origem mandou.
        assert!(v["themes"].as_array().map(|a| !a.is_empty()).unwrap_or(false));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
