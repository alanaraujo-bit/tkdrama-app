//! Pos-producao: transforma episodios do acervo local em clipes curtos,
//! normalizados e marcados — arquivos prontos para o canal vertical do operador.
//!
//! Nao baixa, nao publica, nao escolhe novela: le arquivos que ja estao no disco,
//! recorta a melhor janela, normaliza para 1080x1920 H.264/AAC, aplica a marca
//! d'agua e publica por rename atomico depois de validado — igual ao download.
//!
//! Tres decisoes de peso, tomadas pela regra "o caminho mais simples compativel
//! com os modulos existentes":
//!
//! - **Encoder `libopenh264`, controlado por bitrate.** O FFmpeg empacotado e a
//!   build LGPL (`--disable-libx264 --disable-libx265`): o unico encoder H.264 de
//!   software disponivel e o `libopenh264`, o mesmo que `download::prepare_video`
//!   ja usa. Por isso "crf 20 / preset veryfast" do pedido nao tem como ser
//!   honrado ao pe da letra — nao existe CRF nem preset aqui. O que aquele pedido
//!   protege (qualidade decente, encode rapido, lote rodando sozinho a noite) e
//!   preservado com bitrate fixo. Como a origem e upscale (720x1280 -> 1080x1920),
//!   nao ha detalhe fino a proteger, e um alvo modesto basta.
//!
//! - **Estado e idempotencia derivados do disco.** O `cortes.json` de cada novela
//!   e a verdade sobre "este episodio ja virou clipe" — a mesma filosofia que o
//!   `queue::load` ja declara ("o estado salvo e uma anotacao, o disco e a
//!   verdade"). Nada de banco: a cota do dia e contada varrendo a biblioteca, e um
//!   episodio ja usado como fonte nunca e reusado.
//!
//! - **Marca d'agua pre-renderizada.** A tipografia e decidida uma vez em
//!   `scripts/watermark.py` e commitada como PNG (embutido aqui por `include_bytes!`).
//!   O pipeline so faz overlay em posicao fixa: identica entre um clipe e outro.
//!
//! # Marca embutida de origem
//!
//! Parte do acervo chega com um logotipo que a plataforma grava no proprio video
//! na captura (ReelShort: um "R" que se move entre os cantos ao longo do episodio).
//! E so mais um atributo da captura original que nao pertence ao artefato final —
//! como a resolucao ou o container. O modulo o localiza numa passada offline e, na
//! renderizacao, o remove (padrao), o cobre com a nossa marca, ou o mantem. A
//! escolha e `cortes_source_mark` nos Ajustes; origem sem perfil passa pelo grafo
//! de hoje byte por byte.
//!
//! Duas decisoes de peso, de novo pela regra "o caminho mais simples compativel":
//!
//! - **Deteccao em Rust, nao num script.** `scripts/watermark.py` gera um PNG
//!   commitado uma vez — Python e ferramenta de build, nao dependencia de runtime.
//!   A deteccao, ao contrario, roda sem operador toda noite dentro do lote: poe-la
//!   em Python tornaria Python+numpy dependencia do app empacotado, contra todo o
//!   resto do modulo (Rust + FFmpeg empacotado, auto-contido). Fica em Rust, com o
//!   mesmo FFmpeg gerando frames em cinza cru (`-f rawvideo`) e a correlacao
//!   (ZNCC) escrita a mao — sem crate nova de imagem.
//!
//! - **`remover` por borrao, nao por `delogo`.** A build LGPL empacotada nao inclui
//!   o filtro `delogo` (so `removelogo`, de mascara fixa e sem porta temporal —
//!   inutil para uma marca que anda). Como o pedido de CRF/preset la em cima, a
//!   ferramenta assumida nao existe aqui: `remover` recorta a regiao de cada
//!   segmento, borra com `gblur` (que tem porta temporal) e a sobrepoe de volta,
//!   gateada por `enable='between(t,...)'`. O "rastro suave aceitavel" que o pedido
//!   preve e exatamente o que o borrao deixa. O referencial ReelShort foi extraido
//!   uma vez de um episodio real (720x1280, upscalado ao canvas) e commitado em
//!   `branding/reelshort-mark.png`; a marca sai no mesmo tamanho de canvas em
//!   fontes 720p e 1080p (a plataforma a grava numa proporcao fixa do quadro).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::download;
use crate::manifest;

/// Limiar do detector de cena do FFmpeg (`select='gt(scene,0.3)'`).
const SCENE_THRESHOLD: f64 = 0.3;
/// Duracao alvo do clipe; aceito entre MIN e MAX.
const TARGET: f64 = 45.0;
const MIN_LEN: f64 = 30.0;
const MAX_LEN: f64 = 90.0;
/// Fade de audio na entrada e na saida, para nao estourar no corte.
const FADE: f64 = 0.08;
/// Bitrate de video do fallback `libopenh264` (ver nota do encoder no topo). O
/// NVENC, quando existe, usa VBR proprio (ver `encoder_args`).
const VIDEO_BITRATE: &str = "8000k";
/// No maximo dois clipes da mesma novela por dia.
const MAX_PER_NOVELA_DAY: usize = 2;
/// Peso da recencia no ranking do lote: um episodio recente vale ate +60%.
const RECENCY_WEIGHT: f64 = 0.6;

/// PNG da marca, decidido em `scripts/watermark.py`. Embutido para dispensar
/// plumbing de recurso do Tauri: escrito num temporario na hora do overlay.
const WATERMARK_PNG: &[u8] = include_bytes!("../watermark.png");

/// Selo da estrategia `cobrir`: a LOGO DO APP (mesmo icone do instalador),
/// preparada por `scripts/badge.py`. O texto por extenso nao cobre bem um selo
/// quadrado (o "R" do ReelShort); a logo repete a forma da marca coberta e a tampa
/// por completo, com a identidade do app. Ver `scripts/badge.py`.
const BADGE_PNG: &[u8] = include_bytes!("../branding/badge-logo.png");

// ------------------------------------------------ deteccao da marca de origem
//
// Todas as coordenadas de segmento sao normalizadas 0..1 no canvas de saida
// 1080x1920: medidas DEPOIS do scale/pad (ver `sample_gray_frames`), entao
// mapeiam para a area de conteudo e "aplicam direto em 1080x1920" mesmo quando a
// fonte nao e exatamente 9:16.

/// Resolucao da analise (9:16, 1/8 do canvas). Localizacao com quantizacao de 8px
/// no canvas — folgada, mas a margem de seguranca do patch cobre isso.
const ANALYSIS_W: usize = 135;
const ANALYSIS_H: usize = 240;
/// Amostra a cada 0,5s (o pedido). O tempo do frame i e `i / SAMPLE_FPS`.
const SAMPLE_FPS: f64 = 2.0;
/// Lado do referencial commitado, em px do canvas 1080x1920. Base do tamanho
/// nominal do template na analise: `REF_NATIVE_PX * ANALYSIS_W / 1080`.
const REF_NATIVE_PX: f64 = 132.0;
/// Fracao do quadro dentro da qual duas amostras contam como "o mesmo lugar" e
/// fundem no mesmo segmento; salto maior (a marca troca de canto) vira segmento novo.
/// O pedido diz ~5%; 6% cobre a quantizacao de 8px da analise (8/135 ~ 6%) sem
/// nunca confundir cantos opostos, que distam mais de 50% do quadro.
const MARK_POS_TOL: f64 = 0.06;
/// Ponte sobre quedas curtas de correlacao no mesmo lugar (tipico de fundo claro,
/// onde a marca semitransparente perde contraste): ate este intervalo sem deteccao
/// nao encerra o segmento.
const MARK_MAX_GAP: f64 = 1.0;
/// Segmento mais curto que isto e ruido (um pico isolado de ZNCC no fundo), e cai.
/// A marca real fica parada num canto por muitos segundos.
const MARK_MIN_DUR: f64 = 1.0;
/// Estende cada segmento por meio intervalo de amostragem nas duas pontas, para
/// nao deixar brecha entre a ultima amostra vista e a transicao real.
const MARK_EXTEND: f64 = 0.25;
/// Folga de seguranca em w/h ao cobrir/remover. Menor que os 25% do pedido
/// original: com a interpolacao, quanto menor o buraco, melhor o preenchimento —
/// 15% ainda cobre a quantizacao de 8px da analise.
const MARK_MARGIN: f64 = 0.15;
/// `cobrir`: quanto o selo transborda a caixa detectada, para a placa quadrada
/// tampar o "R" com folga mesmo com erro de localizacao (quantizacao de 8px).
const COVER_SCALE: f64 = 1.15;
/// `cobrir`: padding temporal extra no `enable` de cada selo, alem do `MARK_EXTEND`
/// ja aplicado na deteccao. Faz os selos de segmentos vizinhos se SOBREPOREM na
/// troca de canto: a amostragem de 0,5s nao crava o instante exato do salto do
/// "R", entao a placa do canto novo aparece um tico antes e a do antigo sai um tico
/// depois — o "R" nunca fica descoberto no corte (o preco e ver, por ~0,5s, dois
/// selos durante a troca; melhor que o "R" piscar a mostra).
const COVER_PAD: f64 = 0.4;

/// Perfis por origem, embutidos. include_str! porque e a fonte da verdade
/// commitada; os bytes de cada referencial vem de `reference_png`.
const SOURCE_MARKS_JSON: &str = include_str!("../branding/source-marks.json");

/// Bytes do referencial commitado, por nome de arquivo. `include_bytes!` exige
/// caminho literal, entao cada origem nova entra como um braco aqui alem da linha
/// no `source-marks.json`. Sem braco (ou sem perfil) = sem deteccao.
fn reference_png(name: &str) -> Option<&'static [u8]> {
    match name {
        "reelshort-mark.png" => Some(include_bytes!("../branding/reelshort-mark.png")),
        _ => None,
    }
}

/// O perfil de uma origem: qual referencial usar, em que escalas procurar e com
/// que ZNCC minimo aceitar.
#[derive(Deserialize, Clone, Debug)]
struct SourceProfile {
    reference: String,
    scales: Vec<f64>,
    threshold: f64,
}

/// Le e valida os perfis. Chaves cujo valor nao e um perfil (como o "_nota" de
/// documentacao) sao ignoradas em silencio, em vez de derrubar o mapa inteiro.
fn profiles() -> BTreeMap<String, SourceProfile> {
    let mut out = BTreeMap::new();
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(SOURCE_MARKS_JSON)
    else {
        return out;
    };
    for (k, v) in map {
        if let Ok(p) = serde_json::from_value::<SourceProfile>(v) {
            out.insert(k, p);
        }
    }
    out
}

/// Estrategia para a marca embutida de origem. Persistida como texto em
/// `cortes_source_mark`; `remover` e o padrao e o destino de qualquer valor
/// desconhecido.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMark {
    Remover,
    Cobrir,
    Manter,
}

impl SourceMark {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "cobrir" => Self::Cobrir,
            "manter" => Self::Manter,
            _ => Self::Remover,
        }
    }
}

/// Um segmento da linha do tempo da marca de origem. Tempo em segundos do
/// episodio inteiro; caixa normalizada 0..1 no canvas de saida.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarkSegment {
    pub t0: f64,
    pub t1: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Uma entrada do cache `cortes/marcas.json`: a linha do tempo de um episodio
/// mais a identidade do arquivo que a produziu. Identidade mudou -> reanalisa.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MarkTimeline {
    pub source_name: String,
    /// Origem lida do manifesto no momento da analise (qual perfil se aplicou).
    pub source: String,
    pub source_size: u64,
    pub source_mtime: u64,
    pub duration: f64,
    pub analyzed_at: String,
    pub segments: Vec<MarkSegment>,
}

/// Uma linha do indice `cortes.json`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClipEntry {
    /// Nome do arquivo do clipe, dentro de `cortes/`.
    pub file: String,
    /// Arquivo de origem (episodio ou `trailer.mp4`). E a chave de idempotencia:
    /// o escritor grava aqui e o leitor do conjunto "ja minerado" le exatamente
    /// isto. Nome do arquivo, nunca so o numero, para o trailer tambem caber.
    pub source_episode: String,
    pub start_sec: f64,
    pub end_sec: f64,
    pub score: f64,
    pub generated_at: String,
}

/// Resultado de uma rodada de lote, para a interface e o log.
#[derive(Serialize, Clone, Debug, Default)]
pub struct LoteReport {
    /// Quantos clipes ja existiam para hoje antes desta rodada.
    pub already: usize,
    /// Quantos esta rodada gerou.
    pub generated: usize,
    /// Cota do dia.
    pub quota: usize,
    /// A rodada foi adiada porque havia download em andamento.
    pub deferred: bool,
    /// Falhas por episodio, com motivo — nunca derrubam o lote.
    pub failures: Vec<String>,
}

// -------------------------------------------------------------------- datas

/// Data civil de hoje em UTC, "YYYY-MM-DD". Base da cota diaria e do sufixo do
/// nome do clipe. UTC como o resto do app (ver `manifest::iso_utc`).
fn today() -> String {
    let iso = manifest::now_iso();
    iso.get(..10).unwrap_or("").to_string()
}

/// "YYYY-MM-DD" -> "YYYYMMDD" para o nome `CL-...`.
fn compact_date(iso_date: &str) -> String {
    iso_date.chars().filter(|c| c.is_ascii_digit()).collect()
}

// -------------------------------------------------------------- marca d'agua

/// Extrai um PNG embutido para um temporario reutilizavel (uma vez por processo).
fn embed_file(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(name);
    let precisa = std::fs::metadata(&path)
        .map(|m| m.len() != bytes.len() as u64)
        .unwrap_or(true);
    if precisa {
        std::fs::write(&path, bytes).with_context(|| format!("nao consegui escrever {name}"))?;
    }
    Ok(path)
}

fn watermark_file() -> Result<PathBuf> {
    embed_file("tkdrama-watermark.png", WATERMARK_PNG)
}

/// Selo "N" da estrategia `cobrir`, num temporario.
fn badge_file() -> Result<PathBuf> {
    embed_file("tkdrama-badge-logo.png", BADGE_PNG)
}

// ---------------------------------------------------------------- analise

/// O que a analise mede num episodio.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub duration: f64,
    /// Instantes (s) de mudanca de cena.
    pub scenes: Vec<f64>,
    /// Envelope de energia na banda de voz (dB), uma amostra a cada `RMS_STEP`
    /// segundos. Vazio quando o audio nao pode ser analisado — nesse caso o score
    /// cai para so-cena, sem degradar o episodio inteiro para a janela central.
    pub audio_rms: Vec<f64>,
    /// Trechos de fala (segundos do episodio), derivados do envelope por limiar
    /// adaptativo (ver `speech_runs`). Vazio se o audio nao deu.
    pub speech: Vec<(f64, f64)>,
}

async fn ffmpeg_stderr(args: &[&std::ffi::OsStr]) -> Result<String> {
    let mut cmd = tokio::process::Command::new(download::ffmpeg_path()?);
    cmd.kill_on_drop(true).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x0800_0000);
    }
    let out = cmd.output().await.context("nao consegui executar o FFmpeg")?;
    Ok(String::from_utf8_lossy(&out.stderr).into_owned())
}

fn parse_scene_times(stderr: &str) -> Vec<f64> {
    // `showinfo` imprime uma linha por frame selecionado, com `pts_time:<seg>`.
    let mut out = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.split("pts_time:").nth(1) {
            let raw: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            if let Ok(t) = raw.parse::<f64>() {
                if t.is_finite() && t >= 0.0 {
                    out.push(t);
                }
            }
        }
    }
    out
}

fn parse_rms(stderr: &str) -> Vec<f64> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.split("RMS_level=").nth(1) {
            let raw = rest.trim();
            // Silencio total vem como "-inf": mapeia para um piso baixo em vez
            // de contaminar a variancia com NaN/inf.
            let v = raw.parse::<f64>().unwrap_or(f64::NEG_INFINITY);
            out.push(if v.is_finite() { v } else { -90.0 });
        }
    }
    out
}

/// Passo do envelope de audio, em segundos.
const RMS_STEP: f64 = 0.25;
/// Fala = envelope acima do piso do episodio por esta margem. O piso e o
/// percentil 20 (a trilha de fundo, que em novela nunca cala); um limiar
/// absoluto nao serve porque cada episodio e mixado num nivel.
const SPEECH_ABOVE_FLOOR_DB: f64 = 6.0;
/// Trecho de fala mais curto que isto e ruido de envelope.
const SPEECH_MIN_RUN: f64 = 0.5;

/// Trechos de fala a partir do envelope: amostras acima do piso adaptativo,
/// agrupadas em corridas, descartando as minusculas. Segundos do episodio.
fn speech_runs(rms: &[f64]) -> Vec<(f64, f64)> {
    let mut finite: Vec<f64> = rms.iter().copied().filter(|v| *v > -80.0).collect();
    if finite.len() < 8 {
        return Vec::new();
    }
    finite.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = finite[finite.len() / 5];
    let thr = floor + SPEECH_ABOVE_FLOOR_DB;
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &v) in rms.iter().enumerate() {
        match (v > thr, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let (t0, t1) = (s as f64 * RMS_STEP, i as f64 * RMS_STEP);
                if t1 - t0 >= SPEECH_MIN_RUN {
                    out.push((t0, t1));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        let (t0, t1) = (s as f64 * RMS_STEP, rms.len() as f64 * RMS_STEP);
        if t1 - t0 >= SPEECH_MIN_RUN {
            out.push((t0, t1));
        }
    }
    out
}

/// Mede duracao, cenas e envelope de audio de um episodio.
///
/// Duas passagens de decodificacao (sem encode): cenas por `select+showinfo`,
/// audio por `astats` em janelas de 1s. Falha do audio nao e fatal — devolve
/// `audio_rms` vazio e a selecao usa so a densidade de cena.
pub async fn analyze(path: &Path) -> Result<Analysis> {
    let info = download::probe_video(path).await?;
    if info.duration <= 0.0 {
        bail!("episodio sem duracao mensuravel");
    }

    // Passagem de cena. `showinfo` escreve em loglevel `info`, entao NAO se pode
    // baixar para `error` aqui — senao a lista sai vazia e a janela central
    // mascararia a falha silenciosamente. `-an` acelera: audio nao importa aqui.
    let scene_filter = format!("select='gt(scene,{SCENE_THRESHOLD})',showinfo");
    let scene_args: [&std::ffi::OsStr; 8] = [
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-nostdin"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-i"),
        path.as_os_str(),
        std::ffi::OsStr::new("-vf"),
        std::ffi::OsStr::new(&scene_filter),
        std::ffi::OsStr::new("-f"),
    ];
    let mut scene_args = scene_args.to_vec();
    scene_args.push(std::ffi::OsStr::new("null"));
    scene_args.push(std::ffi::OsStr::new("-"));
    let scenes = parse_scene_times(&ffmpeg_stderr(&scene_args).await?);

    // Passagem de audio: banda de voz (200-3800Hz, onde a fala vive e a trilha
    // pesa menos), reamostra para 8kHz e fatia em 0,25s (`asetnsamples=2000`),
    // tira o RMS de cada fatia. 8kHz basta para um envelope e torna a analise
    // deterministica, independente da taxa de origem.
    let audio_filter =
        "highpass=f=200,lowpass=f=3800,aresample=8000,asetnsamples=2000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level";
    let audio_args: [&std::ffi::OsStr; 10] = [
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-nostdin"),
        std::ffi::OsStr::new("-i"),
        path.as_os_str(),
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:a:0"),
        std::ffi::OsStr::new("-af"),
        std::ffi::OsStr::new(audio_filter),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("null"),
    ];
    let mut audio_args = audio_args.to_vec();
    audio_args.push(std::ffi::OsStr::new("-"));
    let audio_rms = match ffmpeg_stderr(&audio_args).await {
        Ok(text) => parse_rms(&text),
        Err(_) => Vec::new(),
    };
    let speech = speech_runs(&audio_rms);

    Ok(Analysis {
        duration: info.duration,
        scenes,
        audio_rms,
        speech,
    })
}

// ------------------------------------------------ deteccao: amostra e correlacao

/// Roda o FFmpeg e devolve o stdout cru (usado para frames em `rawvideo`).
async fn ffmpeg_stdout(args: &[&OsStr]) -> Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new(download::ffmpeg_path()?);
    cmd.kill_on_drop(true).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x0800_0000);
    }
    let out = cmd.output().await.context("nao consegui executar o FFmpeg")?;
    if !out.status.success() {
        bail!("FFmpeg falhou na amostragem de frames");
    }
    Ok(out.stdout)
}

/// Amostra o episodio inteiro em cinza cru, ja no espaco do canvas de saida.
///
/// O `fps` vem primeiro para so escalar os frames que sobrevivem. O scale/pad e o
/// MESMO do `render_clip`: assim a coordenada normalizada e medida no canvas, nao
/// no quadro da fonte, e mapeia direto para 1080x1920 mesmo quando a fonte tem pad.
async fn sample_gray_frames(path: &Path) -> Result<Vec<u8>> {
    let vf = format!(
        "fps={SAMPLE_FPS},scale=1080:1920:force_original_aspect_ratio=decrease,\
         pad=1080:1920:(ow-iw)/2:(oh-ih)/2,setsar=1,scale={ANALYSIS_W}:{ANALYSIS_H},format=gray"
    );
    let vf = OsStr::new(&vf);
    let args: [&OsStr; 11] = [
        OsStr::new("-hide_banner"),
        OsStr::new("-nostdin"),
        OsStr::new("-an"),
        OsStr::new("-i"),
        path.as_os_str(),
        OsStr::new("-vf"),
        vf,
        OsStr::new("-f"),
        OsStr::new("rawvideo"),
        OsStr::new("-pix_fmt"),
        OsStr::new("gray"),
    ];
    let mut args = args.to_vec();
    args.push(OsStr::new("-"));
    ffmpeg_stdout(&args).await
}

/// Rasteriza o referencial (PNG ja escrito num temporario) em cinza cru no lado
/// pedido. Umas poucas chamadas por deteccao (uma por escala), no inicio.
async fn raster_reference(ref_tmp: &Path, side: usize) -> Result<Vec<u8>> {
    let vf = format!("scale={side}:{side},format=gray");
    let vf = OsStr::new(&vf);
    let args: [&OsStr; 10] = [
        OsStr::new("-hide_banner"),
        OsStr::new("-nostdin"),
        OsStr::new("-i"),
        ref_tmp.as_os_str(),
        OsStr::new("-vf"),
        vf,
        OsStr::new("-f"),
        OsStr::new("rawvideo"),
        OsStr::new("-pix_fmt"),
        OsStr::new("gray"),
    ];
    let mut args = args.to_vec();
    args.push(OsStr::new("-"));
    let raw = ffmpeg_stdout(&args).await?;
    if raw.len() < side * side {
        bail!("referencial rasterizou {} bytes, esperava {}", raw.len(), side * side);
    }
    Ok(raw)
}

/// Um template numa escala: pixels ja com a media subtraida (o denominador do
/// ZNCC nao muda com o brilho do fundo) e a raiz da soma dos quadrados.
struct Template {
    side: usize,
    dev: Vec<f64>,
    ss: f64,
}

fn make_template(raw: &[u8], side: usize) -> Template {
    let n = (side * side) as f64;
    let mean = raw[..side * side].iter().map(|&v| v as f64).sum::<f64>() / n;
    let dev: Vec<f64> = raw[..side * side].iter().map(|&v| v as f64 - mean).collect();
    let ss = dev.iter().map(|d| d * d).sum::<f64>().sqrt().max(1e-9);
    Template { side, dev, ss }
}

/// Imagens integrais (soma e soma dos quadrados) de um frame, para o desvio de
/// qualquer janela em O(1) — o gate de variancia que evita o produto interno caro
/// em regioes chapadas.
struct Integral {
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Integral {
    fn build(frame: &[u8]) -> Self {
        let w = ANALYSIS_W;
        let h = ANALYSIS_H;
        let stride = w + 1;
        let mut sum = vec![0.0; stride * (h + 1)];
        let mut sq = vec![0.0; stride * (h + 1)];
        for y in 0..h {
            let mut row_s = 0.0;
            let mut row_q = 0.0;
            for x in 0..w {
                let v = frame[y * w + x] as f64;
                row_s += v;
                row_q += v * v;
                sum[(y + 1) * stride + (x + 1)] = sum[y * stride + (x + 1)] + row_s;
                sq[(y + 1) * stride + (x + 1)] = sq[y * stride + (x + 1)] + row_q;
            }
        }
        Integral { sum, sq }
    }

    /// (soma, soma-dos-quadrados) da janela `[x, x+side) x [y, y+side)`.
    fn window(&self, x: usize, y: usize, side: usize) -> (f64, f64) {
        let stride = ANALYSIS_W + 1;
        let a = y * stride + x;
        let b = y * stride + (x + side);
        let c = (y + side) * stride + x;
        let d = (y + side) * stride + (x + side);
        let s = self.sum[d] - self.sum[b] - self.sum[c] + self.sum[a];
        let q = self.sq[d] - self.sq[b] - self.sq[c] + self.sq[a];
        (s, q)
    }
}

/// Melhor casamento (ZNCC, canto superior-esquerdo, lado) num frame, sobre todas
/// as escalas. `None` se nenhuma janela teve contraste suficiente para medir.
fn best_match(frame: &[u8], integ: &Integral, tmpls: &[Template]) -> Option<(f64, usize, usize, usize)> {
    let w = ANALYSIS_W;
    let mut best: Option<(f64, usize, usize, usize)> = None;
    for t in tmpls {
        let side = t.side;
        if side >= w || side >= ANALYSIS_H {
            continue;
        }
        let n = (side * side) as f64;
        for y in 0..=(ANALYSIS_H - side) {
            for x in 0..=(w - side) {
                let (s, q) = integ.window(x, y, side);
                let var = q - s * s / n;
                // Regiao chapada nao casa com logotipo nenhum: pula o produto caro.
                if var < 16.0 * n {
                    continue;
                }
                let mut dot = 0.0;
                for j in 0..side {
                    let frow = (y + j) * w + x;
                    let trow = j * side;
                    for i in 0..side {
                        dot += frame[frow + i] as f64 * t.dev[trow + i];
                    }
                }
                let ncc = dot / (var.sqrt() * t.ss);
                if best.map_or(true, |(b, ..)| ncc > b) {
                    best = Some((ncc, x, y, side));
                }
            }
        }
    }
    best
}

/// Uma amostra aceita: onde e quando a marca apareceu.
#[derive(Clone, Copy)]
struct Hit {
    t: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Funde amostras em segmentos: mesma posicao (tolerancia) e sem buraco maior que
/// `MARK_MAX_GAP` continuam o mesmo segmento; salto de posicao ou buraco longo
/// abre outro. Segmentos curtos demais (ruido) caem; os que ficam sao estendidos
/// meio intervalo nas pontas. Caixa do segmento = uniao das amostras que o formam.
fn group_segments(hits: &[Hit], duration: f64) -> Vec<MarkSegment> {
    let mut segs: Vec<MarkSegment> = Vec::new();
    // Acumulador do segmento aberto: uniao da caixa + ultima amostra.
    struct Acc {
        t0: f64,
        t1: f64,
        last_t: f64,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    }
    let mut cur: Option<Acc> = None;

    let close = |acc: Acc, segs: &mut Vec<MarkSegment>| {
        if acc.t1 - acc.t0 + 1.0 / SAMPLE_FPS < MARK_MIN_DUR {
            return; // um pico isolado, nao um segmento
        }
        segs.push(MarkSegment {
            t0: (acc.t0 - MARK_EXTEND).max(0.0),
            t1: (acc.t1 + MARK_EXTEND).min(duration),
            x: acc.x0,
            y: acc.y0,
            w: acc.x1 - acc.x0,
            h: acc.y1 - acc.y0,
        });
    };

    for hit in hits {
        let cx = hit.x + hit.w / 2.0;
        let cy = hit.y + hit.h / 2.0;
        let same_place = |a: &Acc| {
            let acx = (a.x0 + a.x1) / 2.0;
            let acy = (a.y0 + a.y1) / 2.0;
            (acx - cx).abs() <= MARK_POS_TOL && (acy - cy).abs() <= MARK_POS_TOL
        };
        match &mut cur {
            Some(a) if hit.t - a.last_t <= MARK_MAX_GAP + 0.5 / SAMPLE_FPS && same_place(a) => {
                a.t1 = hit.t;
                a.last_t = hit.t;
                a.x0 = a.x0.min(hit.x);
                a.y0 = a.y0.min(hit.y);
                a.x1 = a.x1.max(hit.x + hit.w);
                a.y1 = a.y1.max(hit.y + hit.h);
            }
            _ => {
                if let Some(a) = cur.take() {
                    close(a, &mut segs);
                }
                cur = Some(Acc {
                    t0: hit.t,
                    t1: hit.t,
                    last_t: hit.t,
                    x0: hit.x,
                    y0: hit.y,
                    x1: hit.x + hit.w,
                    y1: hit.y + hit.h,
                });
            }
        }
    }
    if let Some(a) = cur.take() {
        close(a, &mut segs);
    }
    segs
}

/// Detecta a marca de origem no episodio inteiro. Devolve (duracao, segmentos).
/// Segmentos vazios quando a marca nao aparece — nao e erro.
async fn detect(path: &Path, profile: &SourceProfile) -> Result<(f64, Vec<MarkSegment>)> {
    let info = download::probe_video(path).await?;
    if info.duration <= 0.0 {
        bail!("episodio sem duracao mensuravel");
    }
    let bytes = reference_png(&profile.reference)
        .ok_or_else(|| anyhow!("referencial '{}' nao esta embutido", profile.reference))?;

    // Escreve o referencial uma vez e rasteriza uma escala por vez.
    let ref_tmp = std::env::temp_dir().join("tkdrama-source-mark-ref.png");
    tokio::fs::write(&ref_tmp, bytes)
        .await
        .context("nao consegui escrever o referencial temporario")?;
    let nominal = REF_NATIVE_PX * ANALYSIS_W as f64 / 1080.0;
    let mut tmpls = Vec::new();
    for &s in &profile.scales {
        let side = ((nominal * s).round() as usize).clamp(6, ANALYSIS_W.min(ANALYSIS_H) - 1);
        let raw = raster_reference(&ref_tmp, side).await?;
        tmpls.push(make_template(&raw, side));
    }
    if tmpls.is_empty() {
        bail!("perfil sem escalas");
    }

    let frames = sample_gray_frames(path).await?;
    let fsize = ANALYSIS_W * ANALYSIS_H;
    let nframes = frames.len() / fsize;
    let debug = std::env::var_os("TKDRAMA_MARK_DEBUG").is_some();
    let mut hits = Vec::new();
    for f in 0..nframes {
        let frame = &frames[f * fsize..(f + 1) * fsize];
        let integ = Integral::build(frame);
        if let Some((ncc, x, y, side)) = best_match(frame, &integ, &tmpls) {
            if debug {
                eprintln!(
                    "t={:.2} ncc={:.3} pos=({:.3},{:.3}) side={} {}",
                    f as f64 / SAMPLE_FPS,
                    ncc,
                    x as f64 / ANALYSIS_W as f64,
                    y as f64 / ANALYSIS_H as f64,
                    side,
                    if ncc >= profile.threshold { "HIT" } else { "" }
                );
            }
            if ncc >= profile.threshold {
                hits.push(Hit {
                    t: f as f64 / SAMPLE_FPS,
                    x: x as f64 / ANALYSIS_W as f64,
                    y: y as f64 / ANALYSIS_H as f64,
                    w: side as f64 / ANALYSIS_W as f64,
                    h: side as f64 / ANALYSIS_H as f64,
                });
            }
        }
    }
    Ok((info.duration, group_segments(&hits, info.duration)))
}

// -------------------------------------------------- deteccao: cache por episodio

fn marcas_path(novela_dir: &Path) -> PathBuf {
    cortes_dir(novela_dir).join("marcas.json")
}

fn read_marcas(novela_dir: &Path) -> BTreeMap<String, MarkTimeline> {
    std::fs::read_to_string(marcas_path(novela_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Escrita atomica, como o resto do modulo.
fn write_marcas(novela_dir: &Path, map: &BTreeMap<String, MarkTimeline>) -> Result<()> {
    let dir = cortes_dir(novela_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(map)?;
    let tmp = dir.join("marcas.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, marcas_path(novela_dir))?;
    Ok(())
}

/// Linha do tempo da marca de origem de um episodio, com cache por identidade do
/// arquivo (nome + tamanho + mtime). Origem sem perfil devolve vazia (o pipeline
/// segue exatamente como hoje). `force` reanalisa mesmo com cache valido.
///
/// O mesmo episodio minerado duas vezes nunca detecta duas vezes: identidade
/// igual reusa o cache. Trocar o conteudo do arquivo (mesmo nome) muda o tamanho
/// ou o mtime e forca a reanalise.
pub async fn timeline_for(source: &Path, novela_dir: &Path, force: bool) -> Result<Vec<MarkSegment>> {
    let source_name = source
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .context("origem sem nome")?;
    let src = manifest::read(novela_dir).source;
    let Some(profile) = profiles().get(&src).cloned() else {
        return Ok(Vec::new()); // origem nao observada: sem deteccao
    };
    let meta = std::fs::metadata(source)?;
    let size = meta.len();
    let mtime = mtime_secs(source);

    let mut map = read_marcas(novela_dir);
    if !force {
        if let Some(t) = map.get(&source_name) {
            if t.source_size == size && t.source_mtime == mtime {
                return Ok(t.segments.clone());
            }
        }
    }

    let (duration, segments) = detect(source, &profile).await?;
    map.insert(
        source_name.clone(),
        MarkTimeline {
            source_name,
            source: src,
            source_size: size,
            source_mtime: mtime,
            duration,
            analyzed_at: manifest::now_iso(),
            segments: segments.clone(),
        },
    );
    let _ = write_marcas(novela_dir, &map);
    Ok(segments)
}

// ---------------------------------------------------------------- janela

/// Uma janela de corte escolhida no episodio.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: f64,
    pub end: f64,
    /// Quantas fronteiras de cena caem dentro da janela (densidade).
    pub scenes: usize,
    /// Nota editorial da janela (ver `pick_window`); e o `score` do clipe.
    pub quality: f64,
}

impl Window {
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

// Seletor narrativo. O criterio antigo ("maximo de cortes de cena por minuto")
// premiava exatamente o que fica ruim de ver: montagens e recaps de planos soltos,
// comecando no meio de uma fala. Um clipe que prende e uma CENA: abre numa fala,
// tem dialogo continuo, fecha numa pausa natural — e nao comeca na vinheta nem
// termina no teaser do proximo episodio.
/// Descarta a abertura do episodio (vinheta/recap da plataforma).
const HEAD_SKIP: f64 = 6.0;
/// Descarta o final (teaser do proximo episodio, tela de "continua").
const TAIL_SKIP: f64 = 8.0;
/// Silencio menor que isto entre dois trechos de som e pausa DENTRO de uma frase.
const PHRASE_GAP: f64 = 0.7;
/// Respiro antes da primeira fala e depois da ultima.
const LEAD: f64 = 0.4;
const TAIL: f64 = 0.7;
/// Raio para prender inicio/fim a uma fronteira de cena (o pedido: ±1,5s).
const SNAP: f64 = 1.5;
/// Faixa de cortes por minuto que "parece cena": abaixo e plano parado, acima e
/// montagem. Fora dela o peso cai linearmente.
const CUTS_PER_MIN_LO: f64 = 4.0;
const CUTS_PER_MIN_HI: f64 = 18.0;
/// Pausa apos a ultima frase que conta como "fecho natural" cheio.
const PAUSE_FULL: f64 = 1.5;

/// Janela central deterministica: o fallback quando nao ha cenas para se prender.
fn central_window(duration: f64) -> Option<Window> {
    if duration < MIN_LEN {
        return None;
    }
    let len = TARGET.clamp(MIN_LEN, MAX_LEN.min(duration));
    let start = ((duration - len) / 2.0).max(0.0);
    Some(Window {
        start,
        end: (start + len).min(duration),
        scenes: 0,
        quality: 0.0,
    })
}

/// Fallback por densidade de cena: inicio preso a uma fronteira, fim preso a
/// outra o mais perto possivel de +45s, maximizando cortes dentro. So entra
/// quando o audio nao deu para analisar (sem fala nao ha cena para achar).
pub fn pick_window_by_scenes(scenes: &[f64], duration: f64) -> Option<Window> {
    if duration < MIN_LEN {
        return None;
    }
    if scenes.is_empty() {
        return central_window(duration);
    }
    let ceil = MAX_LEN.min(duration);

    // Candidatos a inicio: cada cena que ainda deixa 30s pela frente, mais o 0.
    let mut starts: Vec<f64> = vec![0.0];
    starts.extend(scenes.iter().copied().filter(|&s| s + MIN_LEN <= duration));

    let mut best: Option<Window> = None;
    for &start in &starts {
        let ideal = start + TARGET;
        let lo = start + MIN_LEN;
        let hi = (start + ceil).min(duration);
        if lo > duration {
            continue;
        }
        let end = scenes
            .iter()
            .copied()
            .filter(|&s| s >= lo && s <= hi)
            .min_by(|a, b| {
                (*a - ideal)
                    .abs()
                    .partial_cmp(&(*b - ideal).abs())
                    .unwrap()
            })
            .unwrap_or_else(|| ideal.clamp(lo, hi));
        if end - start < MIN_LEN {
            continue;
        }
        let scenes_in = scenes.iter().filter(|&&s| s > start && s < end).count();
        let cand = Window {
            start,
            end,
            scenes: scenes_in,
            quality: 0.0,
        };
        let melhor = match &best {
            None => true,
            Some(b) => {
                cand.scenes > b.scenes
                    || (cand.scenes == b.scenes
                        && (cand.len() - TARGET).abs() < (b.len() - TARGET).abs())
            }
        };
        if melhor {
            best = Some(cand);
        }
    }
    best.or_else(|| central_window(duration))
}

/// Funde trechos de som separados por pausas curtas em FRASES, dentro da faixa
/// util do episodio. Trechos minusculos (um estalo) caem.
fn phrases(speech: &[(f64, f64)], lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for &(s, e) in speech {
        let s = s.max(lo);
        let e = e.min(hi);
        if e - s < 0.3 {
            continue;
        }
        match out.last_mut() {
            Some(last) if s - last.1 < PHRASE_GAP => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

/// Escolhe a janela como uma cena: comeca numa fala (presa a um corte de cena
/// logo antes, se houver), termina no fim de uma fala (presa a um corte logo
/// depois), e entre as candidatas vence a que tem mais dialogo, ritmo de cena
/// (nem parada, nem montagem), variacao de energia sonora e um fecho em pausa.
///
/// Sem trechos de fala (audio nao analisado) cai no seletor por cenas.
pub fn pick_window(a: &Analysis) -> Option<Window> {
    let duration = a.duration;
    if duration < MIN_LEN {
        return None;
    }
    let lo = if duration - HEAD_SKIP - TAIL_SKIP >= MIN_LEN { HEAD_SKIP } else { 0.0 };
    let hi = if lo > 0.0 { duration - TAIL_SKIP } else { duration };
    let ph = phrases(&a.speech, lo, hi);
    if ph.len() < 2 {
        return pick_window_by_scenes(&a.scenes, duration);
    }

    // Inicio: LEAD antes da frase, ou o corte de cena mais proximo dentro de SNAP
    // antes da fala (o clipe abre no corte, nao no meio do plano anterior).
    let starts: Vec<(f64, bool)> = ph
        .iter()
        .map(|&(s, _)| {
            let raw = (s - LEAD).max(lo);
            let cut = a
                .scenes
                .iter()
                .copied()
                .filter(|&c| c <= s && c >= s - SNAP && c >= lo)
                .fold(None, |acc: Option<f64>, c| Some(acc.map_or(c, |v| v.max(c))));
            match cut {
                Some(c) => (c, true),
                None => (raw, false),
            }
        })
        .collect();
    // Fim: TAIL depois da frase, ou o corte de cena mais proximo dentro de SNAP
    // depois dela. Guarda a pausa ate a proxima frase (o "fecho").
    let ends: Vec<(f64, bool, f64)> = ph
        .iter()
        .enumerate()
        .map(|(i, &(_, e))| {
            let raw = (e + TAIL).min(hi);
            let cut = a
                .scenes
                .iter()
                .copied()
                .filter(|&c| c >= e && c <= e + SNAP && c <= hi)
                .fold(None, |acc: Option<f64>, c| Some(acc.map_or(c, |v| v.min(c))));
            let pause = ph.get(i + 1).map_or(PAUSE_FULL, |n| n.0 - e);
            match cut {
                Some(c) => (c, true, pause),
                None => (raw, false, pause),
            }
        })
        .collect();

    let talk_in = |s: f64, e: f64| -> f64 {
        ph.iter()
            .map(|&(ps, pe)| (pe.min(e) - ps.max(s)).max(0.0))
            .sum::<f64>()
    };

    let mut best: Option<Window> = None;
    for &(s, s_cut) in &starts {
        for &(e, e_cut, pause) in &ends {
            let len = e - s;
            if !(MIN_LEN..=MAX_LEN).contains(&len) {
                continue;
            }
            let cuts = a.scenes.iter().filter(|&&c| c > s && c < e).count();
            let cpm = cuts as f64 / len * 60.0;
            let rhythm = if cpm < CUTS_PER_MIN_LO {
                cpm / CUTS_PER_MIN_LO
            } else if cpm <= CUTS_PER_MIN_HI {
                1.0
            } else {
                (1.0 - (cpm - CUTS_PER_MIN_HI) / 20.0).max(0.0)
            };
            let talk = talk_in(s, e) / len;
            let probe = Window { start: s, end: e, scenes: cuts, quality: 0.0 };
            let audio = (audio_variation(&a.audio_rms, &probe).unwrap_or(0.0) / 8.0).min(1.0);
            let fit = 1.0 - (len - TARGET).abs() / (MAX_LEN - MIN_LEN);
            let close = (pause / PAUSE_FULL).min(1.0);
            let on_cut = (if s_cut { 0.5 } else { 0.0 }) + (if e_cut { 0.5 } else { 0.0 });
            let quality = 3.0 * talk + 2.0 * rhythm + 1.5 * audio + fit + close + on_cut;
            let cand = Window { start: s, end: e, scenes: cuts, quality };
            if best.as_ref().map_or(true, |b| cand.quality > b.quality) {
                best = Some(cand);
            }
        }
    }
    best.or_else(|| pick_window_by_scenes(&a.scenes, duration))
}

/// Desvio-padrao do envelope dentro da janela: a "variacao de energia sonora na
/// janela alvo". `None` quando o audio nao deu para medir (< 2 amostras).
fn audio_variation(rms: &[f64], window: &Window) -> Option<f64> {
    let lo = (window.start / RMS_STEP).floor() as usize;
    let hi = ((window.end / RMS_STEP).ceil() as usize).min(rms.len());
    if hi <= lo {
        return None;
    }
    let slice = &rms[lo..hi];
    if slice.len() < 2 {
        return None;
    }
    let mean = slice.iter().sum::<f64>() / slice.len() as f64;
    let var = slice.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / slice.len() as f64;
    Some(var.sqrt())
}

/// Score do clipe: a nota editorial da janela. Janelas do fallback por cenas
/// (sem fala analisada) recebem uma nota baixa e fixa, mais a variacao sonora
/// comprimida — ficam abaixo de qualquer cena de dialogo decente no ranking do
/// lote, sem sumir quando so elas existem.
pub fn score(analysis: &Analysis, window: &Window) -> f64 {
    if window.quality > 0.0 {
        return window.quality;
    }
    let audio = (audio_variation(&analysis.audio_rms, window).unwrap_or(0.0) / 8.0).min(1.0);
    2.0 + audio
}

// ---------------------------------------------------------------- extracao

/// Base 9:16 do grafo, compartilhada por todos os caminhos: escala pura quando a
/// origem ja e 9:16 (o acervo e), padding minimo quando nao for. `setsar=1` fixa o
/// aspecto de amostra.
const BASE_GRAPH: &str = "[0:v]scale=1080:1920:force_original_aspect_ratio=decrease,\
                pad=1080:1920:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1";

/// Overlay fixo da nossa marca: centro horizontal, no TOPO, a ~11,5% da borda.
///
/// O rodape (uns 25% inferiores) e a lateral direita sao onde TikTok, Reels e
/// Shorts desenham legenda, nome do perfil e botoes — uma marca ali some atras
/// da interface. O topo, logo abaixo das abas, fica livre em todas as tres.
const WATERMARK_POS: &str = "(W-w)/2:H*0.115";

// Embalagem editorial: gancho de abertura e cartao final. Os tempos sao locais ao
// clipe (o grafo comeca em 0).
/// O gancho (episodio + titulo + frase da sinopse) fica em cena por este tempo.
const HOOK_DUR: f64 = 4.0;
const HOOK_FADE_IN: f64 = 0.25;
const HOOK_FADE_OUT: f64 = 0.6;
/// Ultimos segundos do clipe viram o cartao final (escurece, logo, titulo, marca).
const CARD_DUR: f64 = 2.6;
const CARD_FADE: f64 = 0.45;
/// Opacidade do escurecimento do cartao final.
const CARD_DARK: f64 = 0.72;
/// Rosa da flor do app, para o kicker e a marca no cartao (RRGGBB para o drawtext).
const PINK: &str = "0xD6335C";

/// Tipografia do clipe: Montserrat (OFL, commitada em `branding/fonts`). Embutida
/// para o `drawtext` nao depender de fonte instalada na maquina do operador.
const FONT_BOLD_TTF: &[u8] = include_bytes!("../branding/fonts/Montserrat-ExtraBold.ttf");
const FONT_SEMI_TTF: &[u8] = include_bytes!("../branding/fonts/Montserrat-SemiBold.ttf");
/// Flor do app em 480px para o cartao final (ver `scripts/watermark.py`).
const LOGO_CARD_PNG: &[u8] = include_bytes!("../branding/logo-card.png");

// `remover`: a interpolacao (`removelogo`) roda num recorte ao redor da caixa,
// REDUZIDO a este lado maximo. O custo do filtro cresce com o tamanho do buraco
// (raio de interpolacao), nao com o quadro: em 1080p leva minutos por clipe; em
// ~64px e gratis, e o resultado reescalado e indistinguivel — o canto onde a marca
// mora e fundo desfocado.
const REMOVE_LORES_SIDE: i64 = 64;
/// Folga do recorte ao redor da caixa (fracao do lado da caixa), de onde a
/// interpolacao puxa os pixels vizinhos.
const REMOVE_CONTEXT: f64 = 0.4;

fn font_bold_file() -> Result<PathBuf> {
    embed_file("tkdrama-montserrat-extrabold.ttf", FONT_BOLD_TTF)
}
fn font_semi_file() -> Result<PathBuf> {
    embed_file("tkdrama-montserrat-semibold.ttf", FONT_SEMI_TTF)
}
fn logo_card_file() -> Result<PathBuf> {
    embed_file("tkdrama-logo-card.png", LOGO_CARD_PNG)
}

/// Escapa um caminho para valor de opcao dentro de `-filter_complex`. Dois niveis
/// de escape (o do grafo e o do parser de opcoes): `C:/x` vira `C\\:/x`. Barras
/// invertidas viram `/` antes, que o FFmpeg aceita no Windows.
fn ff_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            ':' => out.push_str("\\\\:"),
            ',' => out.push_str("\\\\,"),
            ';' => out.push_str("\\\\;"),
            '\'' => out.push_str("\\\\'"),
            '[' | ']' => {
                out.push_str("\\\\");
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// O que a embalagem escreve na tela. Vem do `manifest.json` da novela.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipMeta {
    pub title: String,
    /// Numero do episodio-fonte; `None` para o trailer.
    pub episode: Option<u32>,
    pub total_episodes: u32,
    /// Uma frase curta da sinopse, quando existe uma que caiba.
    pub hook: Option<String>,
}

/// Le o manifesto e monta o contexto editorial. Sem manifesto (ou sem nome), cai
/// no nome da pasta — o clipe sempre diz de que novela e.
fn clip_meta(novela_dir: &Path, source_name: &str) -> ClipMeta {
    let m = manifest::read(novela_dir);
    let folder = novela_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let title = if m.drama_name.trim().is_empty() { folder } else { m.drama_name.trim().to_string() };
    let episode = m
        .episodes
        .iter()
        .find(|(_, e)| e.file == source_name)
        .map(|(n, _)| *n);
    ClipMeta {
        title,
        episode,
        total_episodes: m.total_episodes,
        hook: hook_line(&m.description),
    }
}

/// Limites do gancho: uma frase da sinopse que caiba em duas linhas grandes.
const HOOK_MIN_CHARS: usize = 24;
const HOOK_MAX_CHARS: usize = 80;

/// Escolhe a frase de gancho: a PRIMEIRA sentenca da sinopse que cabe (a sinopse
/// abre com a premissa, que e o que prende). Sem sentenca curta, corta a primeira
/// na virgula/travessao que ainda caiba e fecha com reticencias. Deterministico,
/// offline, sem IA — o pedido.
pub fn hook_line(description: &str) -> Option<String> {
    let text = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    // Sentencas: fecha em . ! ? seguido de espaco (ou fim). Mantem a pontuacao.
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let fim = matches!(c, '.' | '!' | '?') && chars.get(i + 1).map_or(true, |n| n.is_whitespace());
        if fim {
            sentences.push(cur.trim().to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        sentences.push(cur.trim().to_string());
    }
    let len = |s: &str| s.chars().count();
    if let Some(s) = sentences
        .iter()
        .find(|s| (HOOK_MIN_CHARS..=HOOK_MAX_CHARS).contains(&len(s)))
    {
        return Some(s.clone());
    }
    // Corta a primeira sentenca na ultima virgula/travessao que ainda cabe.
    let first = sentences.first()?;
    let mut best: Option<String> = None;
    for (i, c) in first.char_indices() {
        if matches!(c, ',' | ';' | ':') || first[i..].starts_with(" —") || first[i..].starts_with(" -") {
            let cand = first[..i].trim_end();
            if (HOOK_MIN_CHARS..=HOOK_MAX_CHARS - 1).contains(&len(cand)) {
                best = Some(format!("{cand}…"));
            }
        }
    }
    best
}

/// Quebra em linhas por palavras, com no maximo `max_chars` por linha. Palavra
/// maior que a linha fica sozinha na sua (nunca se parte no meio).
fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand_len = if cur.is_empty() {
            word.chars().count()
        } else {
            cur.chars().count() + 1 + word.chars().count()
        };
        if !cur.is_empty() && cand_len > max_chars {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Um bloco de texto ja ajustado: linhas e corpo da fonte. Tenta o corpo maior
/// primeiro; se estourar `max_lines`, reduz; se ainda estourar, trunca a ultima
/// linha com reticencias — o titulo nunca invade o resto da composicao.
struct TextBlock {
    lines: Vec<String>,
    size: i64,
}

fn fit_text(text: &str, options: &[(i64, usize)], max_lines: usize) -> TextBlock {
    for &(size, max_chars) in options {
        let lines = wrap(text, max_chars);
        if lines.len() <= max_lines {
            return TextBlock { lines, size };
        }
    }
    let &(size, max_chars) = options.last().expect("ao menos uma opcao de corpo");
    let mut lines = wrap(text, max_chars);
    lines.truncate(max_lines);
    if let Some(last) = lines.last_mut() {
        let mut t: String = last.chars().take(max_chars.saturating_sub(1)).collect();
        t = t.trim_end().to_string();
        t.push('…');
        *last = t;
    }
    TextBlock { lines, size }
}

impl TextBlock {
    fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Uma caixa de patch ja no espaco do render: tempo LOCAL (o grafo comeca em 0,
/// deslocado por `window.start`) e pixels do canvas 1080x1920, com a margem.
#[derive(Clone, Debug, PartialEq)]
pub struct PatchBox {
    t0: f64,
    t1: f64,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    /// Centro (px), para a nossa marca se sobrepor centralizada em `cobrir`.
    cx: i64,
    cy: i64,
}

/// Converte os segmentos que tocam a janela em caixas de render: tempo deslocado
/// por `window.start` (o grafo conta do 0) e caixa em px com a margem de seguranca.
/// Segmentos fora da janela somem naturalmente.
fn patch_boxes(timeline: &[MarkSegment], window: &Window) -> Vec<PatchBox> {
    let dur = window.len();
    let mut out = Vec::new();
    for s in timeline {
        let t0 = (s.t0 - window.start).max(0.0);
        let t1 = (s.t1 - window.start).min(dur);
        if t1 <= t0 {
            continue;
        }
        let cxf = (s.x + s.w / 2.0) * 1080.0;
        let cyf = (s.y + s.h / 2.0) * 1920.0;
        let mut w = (s.w * 1080.0 * (1.0 + MARK_MARGIN)).round();
        let mut h = (s.h * 1920.0 * (1.0 + MARK_MARGIN)).round();
        w = w.clamp(8.0, 1080.0);
        h = h.clamp(8.0, 1920.0);
        let x = (cxf - w / 2.0).round().clamp(0.0, 1080.0 - w);
        let y = (cyf - h / 2.0).round().clamp(0.0, 1920.0 - h);
        out.push(PatchBox {
            t0,
            t1,
            x: x as i64,
            y: y as i64,
            w: w as i64,
            h: h as i64,
            cx: cxf.round() as i64,
            cy: cyf.round() as i64,
        });
    }
    out
}

/// Geometria do `remover` para uma caixa: o recorte de contexto (px do canvas), a
/// resolucao reduzida em que a interpolacao roda e a caixa dentro do recorte.
#[derive(Clone, Debug, PartialEq)]
struct RemoveRegion {
    /// Recorte no canvas.
    rx: i64,
    ry: i64,
    rw: i64,
    rh: i64,
    /// Resolucao reduzida do recorte (par, para o yuv420p).
    sw: i64,
    sh: i64,
    /// A caixa da marca dentro do recorte, em px do recorte (para o feather).
    bx: i64,
    by: i64,
    bw: i64,
    bh: i64,
}

fn remove_region(b: &PatchBox) -> RemoveRegion {
    let pad_x = (b.w as f64 * REMOVE_CONTEXT).round() as i64;
    let pad_y = (b.h as f64 * REMOVE_CONTEXT).round() as i64;
    let rx = (b.x - pad_x).max(0);
    let ry = (b.y - pad_y).max(0);
    let rw = ((b.x + b.w + pad_x).min(1080) - rx).max(8);
    let rh = ((b.y + b.h + pad_y).min(1920) - ry).max(8);
    let scale = REMOVE_LORES_SIDE as f64 / rw.max(rh) as f64;
    let even = |v: f64| ((v.round() as i64).max(8) + 1) / 2 * 2;
    let sw = even(rw as f64 * scale);
    let sh = even(rh as f64 * scale);
    RemoveRegion {
        rx,
        ry,
        rw,
        rh,
        sw,
        sh,
        bx: b.x - rx,
        by: b.y - ry,
        bw: b.w,
        bh: b.h,
    }
}

/// Mascara PGM (P5) para o `removelogo`, na resolucao reduzida do recorte:
/// branco na caixa da marca, preto no contexto. Escrita a mao — o formato e um
/// cabecalho de texto mais os bytes; nada de crate de imagem.
fn mask_pgm(r: &RemoveRegion) -> Vec<u8> {
    let sx = r.sw as f64 / r.rw as f64;
    let sy = r.sh as f64 / r.rh as f64;
    let x0 = ((r.bx as f64) * sx).floor() as i64;
    let y0 = ((r.by as f64) * sy).floor() as i64;
    let x1 = (((r.bx + r.bw) as f64) * sx).ceil() as i64;
    let y1 = (((r.by + r.bh) as f64) * sy).ceil() as i64;
    let mut out = format!("P5\n{} {}\n255\n", r.sw, r.sh).into_bytes();
    for y in 0..r.sh {
        for x in 0..r.sw {
            let dentro = x >= x0 && x < x1 && y >= y0 && y < y1;
            out.push(if dentro { 255 } else { 0 });
        }
    }
    out
}

/// Grava (uma vez por geometria) a mascara num temporario e devolve o caminho.
fn mask_file(r: &RemoveRegion) -> Result<PathBuf> {
    let name = format!(
        "tkdrama-mask-{}x{}-{}_{}_{}_{}.pgm",
        r.sw, r.sh, r.bx, r.by, r.bw, r.bh
    );
    embed_file(&name, &mask_pgm(r))
}

/// Tudo que o grafo referencia, ja resolvido: indices de input e caminhos
/// escapados. Separado do `render_clip` para o grafo ser montado (e testado) puro.
#[derive(Clone, Debug, Default)]
pub struct GraphAssets {
    /// Input da nossa marca (None = desligada, ou `cobrir` com selo).
    pub watermark_idx: Option<usize>,
    /// Input do selo de `cobrir`.
    pub badge_idx: Option<usize>,
    /// Input da flor para o cartao final (None = embalagem desligada).
    pub logo_idx: Option<usize>,
    pub font_bold: String,
    pub font_semi: String,
    /// Mascara PGM por caixa de `remover`, na mesma ordem das caixas.
    pub masks: Vec<String>,
    /// Arquivos de texto (escapados) do gancho e do cartao, quando ha embalagem.
    pub texts: Option<GraphTexts>,
}

/// Caminhos dos textos ja gravados, mais o layout decidido (corpo e linhas), para
/// o grafo posicionar cada bloco.
#[derive(Clone, Debug, Default)]
pub struct GraphTexts {
    pub kicker: Option<String>,
    pub title: String,
    pub title_size: i64,
    pub title_lines: usize,
    pub hook: Option<String>,
    pub hook_size: i64,
    pub hook_lines: usize,
    pub card_title: String,
    pub card_title_size: i64,
    pub card_title_lines: usize,
    pub card_sub: Option<String>,
    pub brand: String,
}

/// Expressao de alpha do gancho: entra em `HOOK_FADE_IN`, some ate `HOOK_DUR`.
fn hook_alpha() -> String {
    let hold_end = HOOK_DUR - HOOK_FADE_OUT;
    format!(
        "'if(lt(t,{HOOK_FADE_IN}),t/{HOOK_FADE_IN},if(lt(t,{hold_end}),1,max(0,({HOOK_DUR}-t)/{HOOK_FADE_OUT})))'"
    )
}

/// Expressao de alpha do cartao: entra em `CARD_FADE` a partir de `card_at`.
fn card_alpha(card_at: f64) -> String {
    format!("'if(lt(t,{card_at:.3}),0,min(1,(t-{card_at:.3})/{CARD_FADE}))'")
}

/// Um `drawtext` centralizado, com sombra, alpha por expressao e (opcional)
/// caixa de fundo. `y` e o topo do bloco.
fn drawtext(font: &str, textfile: &str, size: i64, color: &str, y: i64, spacing: i64, alpha: &str, boxcolor: Option<&str>) -> String {
    let mut s = format!(
        "drawtext=fontfile={font}:textfile={textfile}:fontsize={size}:fontcolor={color}\
        :x=(w-text_w)/2:y={y}:line_spacing={spacing}:text_align=C\
        :shadowcolor=black@0.55:shadowx=0:shadowy=4:alpha={alpha}"
    );
    if let Some(bc) = boxcolor {
        s += &format!(":box=1:boxcolor={bc}:boxborderw=18");
    }
    s
}

/// Monta o `-filter_complex` inteiro. Puro (testavel).
///
/// Ordem das camadas, de baixo para cima: base 9:16 -> marca de origem
/// (remover/cobrir) -> gancho -> cartao final -> nossa marca. Audio: loudness
/// normalizada (e so normalizacao do audio existente, nao trilha nova) e fades.
pub fn build_filter(
    strategy: SourceMark,
    boxes: &[PatchBox],
    assets: &GraphAssets,
    dur: f64,
) -> String {
    let mut fc = format!("{BASE_GRAPH}[b0]");
    let mut cur = String::from("b0");
    let mut n = 0usize;
    let mut next = |prefix: &str| {
        n += 1;
        format!("{prefix}{n}")
    };

    let use_marks = strategy != SourceMark::Manter && !boxes.is_empty();
    if use_marks {
        match strategy {
            SourceMark::Remover => {
                for (i, b) in boxes.iter().enumerate() {
                    let r = remove_region(b);
                    let mask = assets.masks.get(i).cloned().unwrap_or_default();
                    let keep = next("k");
                    let src = next("r");
                    let patch = next("p");
                    let m = next("m");
                    let q = next("q");
                    let out = next("b");
                    // Feather: a caixa da marca em branco sobre o recorte, borrada.
                    let sigma = ((r.bw.min(r.bh)) as f64 / 8.0).max(3.0);
                    fc += &format!(
                        ";[{cur}]split=2[{keep}][{src}]\
                         ;[{src}]crop={}:{}:{}:{},scale={}:{},removelogo=f={mask},scale={}:{}:flags=bicubic,gblur=sigma=3[{patch}]\
                         ;color=black:s={}x{}:d={:.3},drawbox=x={}:y={}:w={}:h={}:color=white:t=fill,gblur=sigma={sigma:.1},format=gray[{m}]\
                         ;[{patch}][{m}]alphamerge[{q}]\
                         ;[{keep}][{q}]overlay={}:{}:enable='between(t,{:.3},{:.3})'[{out}]",
                        r.rw, r.rh, r.rx, r.ry, r.sw, r.sh, r.rw, r.rh,
                        r.rw, r.rh, dur + 2.0, r.bx, r.by, r.bw, r.bh,
                        r.rx, r.ry, b.t0, b.t1
                    );
                    cur = out;
                }
            }
            SourceMark::Cobrir => {
                let badge = assets.badge_idx.expect("cobrir com segmentos exige o selo");
                let side = badge_side(boxes);
                let nb = boxes.len();
                fc += &format!(";[{badge}:v]scale={side}:{side}[bd0]");
                if nb > 1 {
                    let outs: String = (0..nb).map(|i| format!("[bd{i}]")).collect();
                    fc += &format!(";[bd0]split={nb}{outs}");
                }
                for (i, b) in boxes.iter().enumerate() {
                    let (x, y) = badge_xy(b, side);
                    let t0 = (b.t0 - COVER_PAD).max(0.0);
                    let t1 = (b.t1 + COVER_PAD).min(dur);
                    let out = next("b");
                    fc += &format!(
                        ";[{cur}][bd{i}]overlay={x}:{y}:enable='between(t,{t0:.3},{t1:.3})'[{out}]"
                    );
                    cur = out;
                }
            }
            SourceMark::Manter => unreachable!(),
        }
    }

    if let Some(t) = &assets.texts {
        // Gancho: kicker (episodio) em caixa rosa, titulo grande, frase da sinopse.
        let alpha = hook_alpha();
        let mut y = 355i64;
        let mut chain: Vec<String> = Vec::new();
        if let Some(k) = &t.kicker {
            chain.push(drawtext(&assets.font_semi, k, 34, "white", y, 0, &alpha, Some(&format!("{PINK}@0.92"))));
            y += 34 + 18 * 2 + 26;
        }
        let title_sp = t.title_size * 15 / 100;
        chain.push(drawtext(&assets.font_bold, &t.title, t.title_size, "white", y, title_sp, &alpha, None));
        y += t.title_lines as i64 * t.title_size + (t.title_lines as i64 - 1).max(0) * title_sp + 22;
        if let Some(h) = &t.hook {
            let sp = t.hook_size * 15 / 100;
            chain.push(drawtext(&assets.font_semi, h, t.hook_size, "white@0.93", y, sp, &alpha, None));
        }
        let out = next("b");
        fc += &format!(";[{cur}]{}[{out}]", chain.join(","));
        cur = out;

        // Cartao final: escurece, flor, titulo, "Episodio X de N", marca.
        let card_at = (dur - CARD_DUR).max(0.0);
        let dk = next("d");
        let out = next("b");
        fc += &format!(
            ";color=black:s=1080x1920:d={:.3},format=rgba,fade=t=in:st={card_at:.3}:d={CARD_FADE}:alpha=1,colorchannelmixer=aa={CARD_DARK}[{dk}]\
             ;[{cur}][{dk}]overlay=0:0[{out}]",
            dur + 2.0
        );
        cur = out;
        if let Some(logo) = assets.logo_idx {
            let lg = next("l");
            let out = next("b");
            fc += &format!(
                ";[{logo}:v]scale=300:300,format=rgba,fade=t=in:st={card_at:.3}:d={CARD_FADE}:alpha=1[{lg}]\
                 ;[{cur}][{lg}]overlay=(W-w)/2:560[{out}]"
            );
            cur = out;
        }
        let alpha = card_alpha(card_at);
        let mut y = 930i64;
        let mut chain: Vec<String> = Vec::new();
        let sp = t.card_title_size * 15 / 100;
        chain.push(drawtext(&assets.font_bold, &t.card_title, t.card_title_size, "white", y, sp, &alpha, None));
        y += t.card_title_lines as i64 * t.card_title_size + (t.card_title_lines as i64 - 1).max(0) * sp + 26;
        if let Some(sub) = &t.card_sub {
            chain.push(drawtext(&assets.font_semi, sub, 40, "white@0.85", y, 0, &alpha, None));
            y += 40 + 70;
        } else {
            y += 40;
        }
        chain.push(drawtext(&assets.font_semi, &t.brand, 50, PINK, y, 0, &alpha, None));
        let out = next("b");
        fc += &format!(";[{cur}]{}[{out}]", chain.join(","));
        cur = out;
    }

    if let Some(wm) = assets.watermark_idx {
        fc += &format!(";[{cur}][{wm}:v]overlay={WATERMARK_POS}[v]");
    } else {
        fc += &format!(";[{cur}]null[v]");
    }

    // Audio: loudness de plataforma (-14 LUFS, o alvo de TikTok/Reels/Shorts) e
    // fades curtos nas pontas. O loudnorm trabalha a 192kHz por dentro; volta a
    // 48kHz para o AAC. Sempre no mesmo grafo (nunca `-af` junto de `-filter_complex`).
    let fade_out_at = (dur - FADE).max(0.0);
    fc += &format!(
        ";[0:a]loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000,afade=t=in:st=0:d={FADE},afade=t=out:st={fade_out_at}:d={FADE}[a]"
    );
    fc
}

/// Lado (px) do selo `cobrir`: cobre o maior segmento por inteiro, com o
/// transbordo de `COVER_SCALE`. A placa e quadrada e centrada, entao o maior lado
/// entre os segmentos (ja com a margem de `patch_boxes`) basta para tampar todos.
fn badge_side(boxes: &[PatchBox]) -> i64 {
    let base = boxes.iter().map(|b| b.w.max(b.h)).max().unwrap_or(0) as f64;
    (base * COVER_SCALE).round().clamp(40.0, 1080.0) as i64
}

/// Canto superior-esquerdo (px), preso ao canvas, para centrar o selo de lado
/// `side` no centro `(cx, cy)` de um segmento.
fn badge_xy(b: &PatchBox, side: i64) -> (i64, i64) {
    let x = (b.cx - side / 2).clamp(0, 1080 - side);
    let y = (b.cy - side / 2).clamp(0, 1920 - side);
    (x, y)
}

// ---------------------------------------------------------------- encoder

/// Encoders H.264 em ordem de preferencia. NVENC (GPU NVIDIA) e muito mais rapido
/// e entrega qualidade melhor a bitrate igual; a build lista o encoder mesmo sem
/// driver, entao a unica prova e tentar. Falhou uma vez, fica marcado para o
/// resto do processo e todo clipe seguinte vai direto ao `libopenh264` (o unico
/// encoder H.264 de software da build LGPL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoder {
    Nvenc,
    OpenH264,
}

static NVENC_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn encoder_args(e: Encoder) -> Vec<&'static str> {
    match e {
        // VBR com teto de qualidade: ~9 Mbps de alvo, ate 13 no pico. Generoso de
        // proposito: as plataformas re-encodam em cima, e cada geracao perde.
        Encoder::Nvenc => vec![
            "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", "21",
            "-b:v", "9M", "-maxrate", "13M", "-bufsize", "20M", "-profile:v", "high",
        ],
        Encoder::OpenH264 => vec!["-c:v", "libopenh264", "-b:v", VIDEO_BITRATE],
    }
}

/// Os candidatos a tentar nesta ordem.
fn encoders() -> Vec<Encoder> {
    if NVENC_BROKEN.load(std::sync::atomic::Ordering::Relaxed) {
        vec![Encoder::OpenH264]
    } else {
        vec![Encoder::Nvenc, Encoder::OpenH264]
    }
}

// ---------------------------------------------------------------- render

/// Arquivos de texto do gancho e do cartao, gravados em temporarios com um
/// prefixo unico por render (varios clipes podem correr no mesmo processo).
struct TextFiles {
    texts: GraphTexts,
    paths: Vec<PathBuf>,
}

impl Drop for TextFiles {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Decide o layout dos textos e os grava. O titulo tenta 72px (ate 3 linhas),
/// depois 60px; a frase de gancho 44px em ate 2 linhas.
fn write_texts(meta: &ClipMeta, tag: &str) -> Result<TextFiles> {
    let dir = std::env::temp_dir();
    let mut paths = Vec::new();
    let mut save = |name: &str, content: &str| -> Result<String> {
        let p = dir.join(format!("tkdrama-txt-{tag}-{name}.txt"));
        std::fs::write(&p, content).with_context(|| format!("nao consegui escrever {}", p.display()))?;
        let esc = ff_path(&p);
        paths.push(p);
        Ok(esc)
    };

    let title = fit_text(&meta.title, &[(72, 20), (60, 24)], 3);
    let hook = meta.hook.as_deref().map(|h| fit_text(h, &[(44, 36)], 2));
    let card_title = fit_text(&meta.title, &[(60, 24), (50, 30)], 3);
    let kicker = meta.episode.map(|n| format!("EPISÓDIO {n}"));
    let card_sub = match (meta.episode, meta.total_episodes) {
        (Some(n), total) if total > 0 => Some(format!("Episódio {n} de {total}")),
        (Some(n), _) => Some(format!("Episódio {n}")),
        (None, total) if total > 0 => Some(format!("{total} episódios")),
        _ => None,
    };

    let texts = GraphTexts {
        kicker: match &kicker {
            Some(k) => Some(save("kicker", k)?),
            None => None,
        },
        title: save("title", &title.text())?,
        title_size: title.size,
        title_lines: title.lines.len(),
        hook: match &hook {
            Some(h) => Some(save("hook", &h.text())?),
            None => None,
        },
        hook_size: hook.as_ref().map(|h| h.size).unwrap_or(44),
        hook_lines: hook.as_ref().map(|h| h.lines.len()).unwrap_or(0),
        card_title: save("ctitle", &card_title.text())?,
        card_title_size: card_title.size,
        card_title_lines: card_title.lines.len(),
        card_sub: match &card_sub {
            Some(s) => Some(save("csub", s)?),
            None => None,
        },
        brand: save("brand", "Noveleiras de plantão")?,
    };
    Ok(TextFiles { texts, paths })
}

/// Recorta, normaliza para 1080x1920, trata a marca de origem, aplica a
/// embalagem (gancho + cartao) e a nossa marca — tudo num unico passo de FFmpeg.
/// Publica por rename atomico apos validacao profunda.
async fn render_clip(
    episode: &Path,
    window: &Window,
    dest: &Path,
    watermark: bool,
    strategy: SourceMark,
    timeline: &[MarkSegment],
    meta: Option<&ClipMeta>,
) -> Result<()> {
    let tmp = download::video_part_path(dest);
    let _ = tokio::fs::remove_file(&tmp).await;
    let dur = window.len();

    let boxes = patch_boxes(timeline, window);
    // `cobrir` (quando ha o que cobrir) usa a logo e DISPENSA a marca fixa — a
    // propria logo assina; nunca dois elementos nossos ao mesmo tempo.
    let use_cobrir_badge = strategy == SourceMark::Cobrir && !boxes.is_empty();
    let use_remover = strategy == SourceMark::Remover && !boxes.is_empty();

    // Inputs extras, na ordem em que entram na linha de comando.
    let mut inputs: Vec<(PathBuf, bool)> = Vec::new(); // (caminho, e imagem em loop)
    let mut assets = GraphAssets {
        font_bold: ff_path(&font_bold_file()?),
        font_semi: ff_path(&font_semi_file()?),
        ..Default::default()
    };
    if watermark && !use_cobrir_badge {
        inputs.push((watermark_file()?, false));
        assets.watermark_idx = Some(inputs.len());
    }
    if use_cobrir_badge {
        inputs.push((badge_file()?, false));
        assets.badge_idx = Some(inputs.len());
    }
    if use_remover {
        for b in &boxes {
            assets.masks.push(ff_path(&mask_file(&remove_region(b))?));
        }
    }
    let tag = format!(
        "{}-{}",
        std::process::id(),
        dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
            .chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()
    );
    let _texts = match meta {
        Some(m) => {
            let tf = write_texts(m, &tag)?;
            inputs.push((logo_card_file()?, true));
            assets.logo_idx = Some(inputs.len());
            assets.texts = Some(tf.texts.clone());
            Some(tf)
        }
        None => None,
    };
    let filter = build_filter(strategy, &boxes, &assets, dur);

    let mut last_err: Option<anyhow::Error> = None;
    for enc in encoders() {
        let mut cmd = tokio::process::Command::new(download::ffmpeg_path()?);
        cmd.kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            // `-ss` antes do `-i`: busca rapida e, com re-encode, precisa ao frame.
            .arg("-ss")
            .arg(format!("{:.3}", window.start))
            .arg("-t")
            .arg(format!("{dur:.3}"))
            .arg("-i")
            .arg(episode);
        for (path, looped) in &inputs {
            if *looped {
                // Imagem em loop limitado a duracao do clipe: precisa produzir
                // frames ao longo do tempo para o fade do cartao existir.
                cmd.arg("-loop").arg("1").arg("-framerate").arg("24").arg("-t").arg(format!("{dur:.3}"));
            }
            cmd.arg("-i").arg(path);
        }
        cmd.arg("-filter_complex")
            .arg(&filter)
            .arg("-map")
            .arg("[v]")
            .arg("-map")
            .arg("[a]")
            .args(encoder_args(enc))
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("192k")
            .arg("-movflags")
            .arg("+faststart")
            .arg(&tmp);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.as_std_mut().creation_flags(0x0800_0000);
        }

        let out = cmd.output().await.context("nao consegui executar o FFmpeg")?;
        if out.status.success() {
            last_err = None;
            break;
        }
        let _ = tokio::fs::remove_file(&tmp).await;
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if enc == Encoder::Nvenc {
            NVENC_BROKEN.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("NVENC indisponivel ({msg}); usando libopenh264 daqui em diante");
        }
        last_err = Some(anyhow!("FFmpeg falhou ao gerar o clipe: {msg}"));
    }
    if let Some(e) = last_err {
        return Err(e);
    }

    // Mesma validacao profunda do download: so publica MP4 real, H.264/AAC, com
    // faststart e sem truncamento.
    download::validate_video(&tmp, true)
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e
        })?;
    if dest.exists() {
        tokio::fs::remove_file(dest).await?;
    }
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

// ---------------------------------------------------------------- indice

fn cortes_dir(novela_dir: &Path) -> PathBuf {
    novela_dir.join("cortes")
}

fn index_path(novela_dir: &Path) -> PathBuf {
    cortes_dir(novela_dir).join("cortes.json")
}

/// Le o indice de uma novela (vazio quando ainda nao ha cortes).
pub fn read_index(novela_dir: &Path) -> Vec<ClipEntry> {
    std::fs::read_to_string(index_path(novela_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Grava o indice por `.tmp` + rename, como o manifesto.
fn write_index(novela_dir: &Path, entries: &[ClipEntry]) -> Result<()> {
    let dir = cortes_dir(novela_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(entries)?;
    let tmp = dir.join("cortes.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, index_path(novela_dir))?;
    Ok(())
}

/// Conjunto de origens ja usadas nesta novela (chave de idempotencia).
fn mined(novela_dir: &Path) -> BTreeSet<String> {
    read_index(novela_dir)
        .into_iter()
        .map(|e| e.source_episode)
        .collect()
}

/// Quantos clipes desta novela foram gerados na data dada.
fn day_count(novela_dir: &Path, day: &str) -> usize {
    read_index(novela_dir)
        .into_iter()
        .filter(|e| e.generated_at.starts_with(day))
        .count()
}

/// Proximo indice global `CL-<data>-NN` do dia, varrendo a biblioteca inteira.
/// Global (nao por novela) porque o lote e diario: os NN correm pelo conjunto.
fn next_index(root: &Path, day: &str) -> usize {
    let compact = compact_date(day);
    let prefixo = format!("CL-{compact}-");
    let mut max = 0usize;
    for novela in novela_dirs(root) {
        for e in read_index(&novela) {
            if let Some(rest) = e.file.split(&prefixo).nth(1) {
                let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(v) = n.parse::<usize>() {
                    max = max.max(v);
                }
            }
        }
    }
    max + 1
}

// ---------------------------------------------------------------- biblioteca

/// Um episodio (ou trailer) candidato a virar clipe.
struct Candidate {
    novela_dir: PathBuf,
    novela_name: String,
    source: PathBuf,
    /// Nome do arquivo de origem (chave de idempotencia).
    source_name: String,
    /// mtime, para o peso de recencia.
    mtime: u64,
}

fn mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Subpastas de `root` que contem pelo menos um arquivo de video de episodio.
fn novela_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !manifest::episode_files(p).is_empty())
        .collect();
    out.sort();
    out
}

/// Nome final do arquivo de clipe.
fn clip_filename(novela_name: &str, day: &str, index: usize) -> String {
    format!("{novela_name} - CL-{}-{index:02}.mp4", compact_date(day))
}

/// O que o operador escolhe para cada render. Um so valor para passar adiante,
/// em vez de tres flags soltas em cada assinatura.
#[derive(Clone, Copy, Debug)]
pub struct ClipOptions {
    pub watermark: bool,
    pub source_mark: SourceMark,
    /// Gancho de abertura e cartao final.
    pub packaging: bool,
}

/// Analisa, escolhe a janela e renderiza um clipe de `source` em `dest`, sem
/// tocar no indice da novela. E o miolo de `make_clip` e o que a CLI usa com
/// `--saida` (QA de render sem "queimar" o episodio do pool do lote).
pub async fn make_clip_to(source: &Path, dest: &Path, opts: &ClipOptions) -> Result<ClipEntry> {
    let novela_dir = source
        .parent()
        .context("arquivo de origem sem pasta")?
        .to_path_buf();
    let source_name = source
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .context("origem sem nome")?;

    let analysis = analyze(source).await?;
    let window = pick_window(&analysis).context("episodio curto demais para um clipe de 30s")?;
    let sc = score(&analysis, &window);

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Deteccao da marca de origem (com cache); falha nao derruba o clipe, so cai
    // para 'manter' (linha do tempo vazia). Origem sem perfil ja devolve vazia.
    let timeline = match timeline_for(source, &novela_dir, false).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("deteccao de marca falhou em {source_name}: {e}; usando 'manter'");
            Vec::new()
        }
    };
    let meta = if opts.packaging { Some(clip_meta(&novela_dir, &source_name)) } else { None };
    render_clip(source, &window, dest, opts.watermark, opts.source_mark, &timeline, meta.as_ref()).await?;

    Ok(ClipEntry {
        file: dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        source_episode: source_name,
        start_sec: (window.start * 1000.0).round() / 1000.0,
        end_sec: (window.end * 1000.0).round() / 1000.0,
        score: (sc * 1000.0).round() / 1000.0,
        generated_at: manifest::now_iso(),
    })
}

/// Gera um clipe manual de um arquivo do acervo (CLI `cortes um`): mesmo
/// caminho do lote, mas para um episodio escolhido. Publica em `cortes/` da
/// novela e registra no indice.
pub async fn make_clip(source: &Path, opts: &ClipOptions) -> Result<ClipEntry> {
    let novela_dir = source
        .parent()
        .context("arquivo de origem sem pasta")?
        .to_path_buf();
    let novela_name = novela_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .context("pasta da novela sem nome")?;
    let root = novela_dir.parent().context("biblioteca sem raiz")?.to_path_buf();
    let source_name = source
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .context("origem sem nome")?;

    if mined(&novela_dir).contains(&source_name) {
        bail!("{source_name} ja foi usado como fonte de clipe");
    }

    let day = today();
    let index = next_index(&root, &day);
    let file = clip_filename(&novela_name, &day, index);
    let dest = cortes_dir(&novela_dir).join(&file);
    let entry = make_clip_to(source, &dest, opts).await?;

    let mut idx = read_index(&novela_dir);
    idx.push(entry.clone());
    write_index(&novela_dir, &idx)?;
    Ok(entry)
}

// ---------------------------------------------------------------- resumo

/// Um clipe ja no disco, para a interface listar.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClipInfo {
    pub novela: String,
    pub file: String,
    pub source_episode: String,
    pub start_sec: f64,
    pub end_sec: f64,
    pub score: f64,
    pub generated_at: String,
    /// Caminho absoluto do arquivo, para "Abrir pasta".
    pub path: String,
}

/// Resumo do dia para a aba Cortes.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub day: String,
    pub generated: usize,
    /// Todos os clipes de hoje, mais recentes primeiro.
    pub clips: Vec<ClipInfo>,
}

/// Varre a biblioteca e monta o resumo dos clipes gerados hoje.
pub fn summary(root: &Path) -> Summary {
    let day = today();
    let mut clips = Vec::new();
    for novela_dir in novela_dirs(root) {
        let novela = novela_dir.file_name().unwrap_or_default().to_string_lossy().to_string();
        for e in read_index(&novela_dir) {
            if !e.generated_at.starts_with(&day) {
                continue;
            }
            clips.push(ClipInfo {
                novela: novela.clone(),
                path: cortes_dir(&novela_dir).join(&e.file).to_string_lossy().to_string(),
                file: e.file,
                source_episode: e.source_episode,
                start_sec: e.start_sec,
                end_sec: e.end_sec,
                score: e.score,
                generated_at: e.generated_at,
            });
        }
    }
    // Mais recentes primeiro (o ISO ordena lexicograficamente).
    clips.sort_by(|a, b| b.generated_at.cmp(&a.generated_at));
    Summary {
        day,
        generated: clips.len(),
        clips,
    }
}

// ---------------------------------------------------------------- lote

/// Reune os candidatos elegiveis da biblioteca (nao minerados, novela abaixo do
/// teto do dia), ja ordenados por recencia.
fn gather_candidates(root: &Path, day: &str) -> Vec<Candidate> {
    let mut cands = Vec::new();
    for novela_dir in novela_dirs(root) {
        if day_count(&novela_dir, day) >= MAX_PER_NOVELA_DAY {
            continue;
        }
        let ja = mined(&novela_dir);
        let novela_name = novela_dir.file_name().unwrap_or_default().to_string_lossy().to_string();

        let mut fontes: Vec<(PathBuf, String)> = manifest::episode_files(&novela_dir)
            .into_values()
            .map(|nome| (novela_dir.join(&nome), nome))
            .collect();
        // O trailer tambem e candidato.
        let trailer = novela_dir.join(manifest::TRAILER_FILE);
        if trailer.is_file() {
            fontes.push((trailer, manifest::TRAILER_FILE.to_string()));
        }

        for (source, source_name) in fontes {
            if ja.contains(&source_name) {
                continue;
            }
            cands.push(Candidate {
                novela_dir: novela_dir.clone(),
                novela_name: novela_name.clone(),
                mtime: mtime_secs(&source),
                source,
                source_name,
            });
        }
    }
    // Recencia primeiro: e barato e a recencia pesa de verdade no criterio.
    cands.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    cands
}

/// Roda o lote: completa a cota do dia sem duplicar, no maximo 2 por novela,
/// em novelas distintas na medida do possivel. Falha num episodio nao derruba o
/// lote — o motivo entra no relatorio e a rodada segue.
///
/// Nao verifica agenda nem download em andamento: quem chama (o laco de fundo)
/// decide isso. Aqui so ha a producao.
pub async fn run_lote(root: &Path, quota: usize, opts: &ClipOptions) -> LoteReport {
    let day = today();
    let mut report = LoteReport {
        quota,
        ..Default::default()
    };
    // Cota ja cumprida hoje: a base da idempotencia. Reexecutar completa o que
    // falta, sem refazer.
    report.already = novela_dirs(root).iter().map(|d| day_count(d, &day)).sum();
    let mut faltam = quota.saturating_sub(report.already);
    if faltam == 0 {
        return report;
    }

    // Contagem por novela ja feita hoje, para respeitar o teto de 2 entre rodadas.
    let mut por_novela: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for d in novela_dirs(root) {
        por_novela.insert(d.clone(), day_count(&d, &day));
    }

    let candidates = gather_candidates(root, &day);
    if candidates.is_empty() {
        return report;
    }

    // Analisa um pool limitado dos mais recentes: encode e caro, e so precisamos
    // de ~cota clipes. Quando os recentes acabam, os antigos nunca minerados
    // sobem sozinhos — comportamento esperado, sem aviso.
    let cap = (faltam.saturating_mul(4)).max(40);
    let mut scored: Vec<(f64, u64, Candidate, Window)> = Vec::new();
    let mut recent_max = 0u64;
    for cand in candidates.into_iter().take(cap) {
        recent_max = recent_max.max(cand.mtime);
        match analyze(&cand.source).await {
            Ok(a) => match pick_window(&a) {
                Some(w) => {
                    let s = score(&a, &w);
                    scored.push((s, cand.mtime, cand, w));
                }
                None => report
                    .failures
                    .push(format!("{}: episodio curto demais", cand.source_name)),
            },
            Err(e) => report.failures.push(format!("{}: {e}", cand.source_name)),
        }
    }

    // Ranking final: score do conteudo ponderado pela recencia normalizada.
    let recent_min = scored.iter().map(|(_, m, _, _)| *m).min().unwrap_or(0);
    let span = recent_max.saturating_sub(recent_min).max(1) as f64;
    scored.sort_by(|a, b| {
        let ra = 1.0 + RECENCY_WEIGHT * ((a.1 - recent_min) as f64 / span);
        let rb = 1.0 + RECENCY_WEIGHT * ((b.1 - recent_min) as f64 / span);
        (b.0 * rb).partial_cmp(&(a.0 * ra)).unwrap()
    });

    // Selecao em varreduras por teto crescente: a primeira poe no maximo 1 clipe
    // por novela (espalha ao maximo, "novelas distintas"), a segunda so entao
    // completa a segunda cota de uma novela. Dentro de cada varredura, o mais
    // bem pontuado primeiro.
    let mut usados = vec![false; scored.len()];
    for teto in 1..=MAX_PER_NOVELA_DAY {
        for (i, (sc, _m, cand, window)) in scored.iter().enumerate() {
            if faltam == 0 {
                break;
            }
            if usados[i] {
                continue;
            }
            if por_novela.get(&cand.novela_dir).copied().unwrap_or(0) >= teto {
                continue;
            }
            // Deteccao da marca de origem (com cache), so para os selecionados.
            // Falha nao pula o clipe: cai para 'manter' e o motivo entra no relatorio.
            let timeline = match timeline_for(&cand.source, &cand.novela_dir, false).await {
                Ok(t) => t,
                Err(e) => {
                    report
                        .failures
                        .push(format!("{}: deteccao de marca falhou ({e}); usando 'manter'", cand.source_name));
                    Vec::new()
                }
            };
            match generate_one(root, cand, window, *sc, opts, &timeline).await {
                Ok(()) => {
                    usados[i] = true;
                    *por_novela.entry(cand.novela_dir.clone()).or_insert(0) += 1;
                    report.generated += 1;
                    faltam -= 1;
                }
                Err(e) => {
                    usados[i] = true; // nao reincidir na segunda varredura
                    report.failures.push(format!("{}: {e}", cand.source_name));
                }
            }
        }
        if faltam == 0 {
            break;
        }
    }
    report
}

/// Gera e registra um unico clipe do candidato. Extraido para as varreduras de
/// selecao do lote nao duplicarem o corpo.
async fn generate_one(
    root: &Path,
    cand: &Candidate,
    window: &Window,
    sc: f64,
    opts: &ClipOptions,
    timeline: &[MarkSegment],
) -> Result<()> {
    let day = today();
    let index = next_index(root, &day);
    let file = clip_filename(&cand.novela_name, &day, index);
    let dest = cortes_dir(&cand.novela_dir).join(&file);
    std::fs::create_dir_all(cortes_dir(&cand.novela_dir))?;
    let meta = if opts.packaging { Some(clip_meta(&cand.novela_dir, &cand.source_name)) } else { None };
    render_clip(&cand.source, window, &dest, opts.watermark, opts.source_mark, timeline, meta.as_ref()).await?;
    let entry = ClipEntry {
        file,
        source_episode: cand.source_name.clone(),
        start_sec: (window.start * 1000.0).round() / 1000.0,
        end_sec: (window.end * 1000.0).round() / 1000.0,
        score: (sc * 1000.0).round() / 1000.0,
        generated_at: manifest::now_iso(),
    };
    let mut idx = read_index(&cand.novela_dir);
    idx.push(entry);
    write_index(&cand.novela_dir, &idx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(duration: f64, scenes: Vec<f64>, speech: Vec<(f64, f64)>) -> Analysis {
        // RMS "vivo": alterna para a variacao sonora nao ser zero.
        let n = (duration / RMS_STEP).ceil() as usize;
        let audio_rms = (0..n).map(|i| if i % 2 == 0 { -30.0 } else { -18.0 }).collect();
        Analysis { duration, scenes, audio_rms, speech }
    }

    #[test]
    fn janela_curta_demais_nao_gera() {
        assert!(pick_window(&analysis(20.0, vec![], vec![])).is_none());
        assert!(pick_window_by_scenes(&[], 20.0).is_none());
        assert!(central_window(20.0).is_none());
    }

    #[test]
    fn sem_fala_nem_cenas_cai_na_janela_central() {
        let w = pick_window(&analysis(120.0, vec![], vec![])).unwrap();
        assert_eq!(w.scenes, 0);
        assert!((w.len() - TARGET).abs() < 0.01);
        assert!((w.start - (120.0 - TARGET) / 2.0).abs() < 0.01);
    }

    #[test]
    fn fallback_por_cenas_respeita_os_limites() {
        let cenas: Vec<f64> = (0..90).map(|i| i as f64).collect();
        let w = pick_window_by_scenes(&cenas, 90.0).unwrap();
        assert!(w.len() >= MIN_LEN - 0.01 && w.len() <= MAX_LEN + 0.01);
        assert!(w.start == 0.0 || cenas.contains(&w.start));
    }

    #[test]
    fn fallback_escolhe_a_janela_mais_densa() {
        let cenas = vec![2.0, 48.0, 49.0, 50.0, 51.0, 52.0, 85.0];
        let w = pick_window_by_scenes(&cenas, 100.0).unwrap();
        assert!(w.start <= 48.0 && w.end >= 52.0);
        assert!(w.scenes >= 4);
    }

    #[test]
    fn frases_fundem_pausas_curtas_e_respeitam_a_faixa() {
        let speech = vec![(2.0, 4.0), (4.3, 6.0), (8.0, 9.0), (9.1, 9.2), (150.0, 160.0)];
        let ph = phrases(&speech, 3.0, 140.0);
        // (2,4)+(4.3,6) fundem e sao recortadas em 3.0; (9.1,9.2) e minuscula.
        assert_eq!(ph, vec![(3.0, 6.0), (8.0, 9.0)]);
    }

    #[test]
    fn janela_narrativa_abre_numa_fala_e_fecha_numa_pausa() {
        // Episodio de 180s. Fala continua de 40 a 90 (dialogo denso, cortes a cada
        // 5s), silencio ate 100, e falas esparsas depois.
        let mut speech = Vec::new();
        let mut t = 40.0;
        while t < 90.0 {
            speech.push((t, t + 2.5));
            t += 3.0;
        }
        speech.extend([(100.0, 103.0), (120.0, 122.0), (150.0, 152.0)]);
        let scenes: Vec<f64> = (0..36).map(|i| i as f64 * 5.0).collect();
        let w = pick_window(&analysis(180.0, scenes, speech)).unwrap();
        assert!(w.quality > 0.0);
        // Comeca no corte de cena logo antes da primeira fala (40 - LEAD -> corte em 40).
        assert!((w.start - 40.0).abs() < 1e-6, "inicio preso ao corte antes da fala: {}", w.start);
        // Termina depois da ultima frase do bloco (89.5) e antes da fala seguinte (100).
        assert!(w.end > 89.5 && w.end <= 100.0, "fim no fecho natural: {}", w.end);
        assert!(w.len() >= MIN_LEN && w.len() <= MAX_LEN);
    }

    #[test]
    fn janela_narrativa_evita_abertura_e_teaser() {
        // Fala densa nos primeiros 40s (vinheta/recap) e nos ultimos 40s; um bloco
        // de dialogo no meio. O clipe tem que ficar no meio.
        let mut speech = Vec::new();
        for i in 0..12 {
            let t = i as f64 * 3.0;
            speech.push((t, t + 2.0));
        }
        for i in 0..14 {
            let t = 60.0 + i as f64 * 3.0;
            speech.push((t, t + 2.0));
        }
        for i in 0..12 {
            let t = 145.0 + i as f64 * 3.0;
            speech.push((t, t + 2.0));
        }
        let scenes: Vec<f64> = (0..36).map(|i| i as f64 * 5.0).collect();
        let w = pick_window(&analysis(180.0, scenes, speech)).unwrap();
        assert!(w.start >= HEAD_SKIP, "nao comeca na abertura: {}", w.start);
        assert!(w.end <= 180.0 - TAIL_SKIP, "nao entra no teaser: {}", w.end);
        assert!(w.start >= 55.0 && w.end <= 110.0, "fica no bloco do meio: {}..{}", w.start, w.end);
    }

    #[test]
    fn montagem_perde_para_cena_com_dialogo() {
        // Dois blocos de fala iguais; o primeiro tem corte a cada segundo
        // (montagem), o segundo a cada 6s (cena). O segundo tem que vencer.
        let mut speech = Vec::new();
        for i in 0..15 {
            let t = 10.0 + i as f64 * 3.0;
            speech.push((t, t + 2.4));
        }
        for i in 0..15 {
            let t = 100.0 + i as f64 * 3.0;
            speech.push((t, t + 2.4));
        }
        let mut scenes: Vec<f64> = (10..56).map(|i| i as f64).collect();
        scenes.extend((0..8).map(|i| 100.0 + i as f64 * 6.0));
        let w = pick_window(&analysis(200.0, scenes, speech)).unwrap();
        assert!(w.start >= 95.0, "a cena vence a montagem: {}..{}", w.start, w.end);
    }

    #[test]
    fn variacao_de_audio_e_desvio_padrao() {
        let a = Analysis {
            duration: 10.0,
            scenes: vec![],
            audio_rms: vec![-30.0, -10.0, -30.0, -10.0, -30.0, -10.0, -30.0, -10.0, -30.0, -10.0],
            speech: vec![],
        };
        let w = Window { start: 0.0, end: 2.5, scenes: 0, quality: 0.0 };
        let v = audio_variation(&a.audio_rms, &w).unwrap();
        assert!((v - 10.0).abs() < 0.5); // alterna +-10 em torno de -20
    }

    #[test]
    fn fala_por_limiar_adaptativo() {
        // Piso a -30 com um bloco de fala a -20 entre 2s e 4s e um pico isolado.
        let mut rms = vec![-30.0; 40];
        for i in 8..16 {
            rms[i] = -20.0;
        }
        rms[30] = -15.0; // 0,25s: curto demais
        let runs = speech_runs(&rms);
        assert_eq!(runs, vec![(2.0, 4.0)]);
        // Mixado 10dB mais baixo, mesmo resultado (limiar e relativo).
        let low: Vec<f64> = rms.iter().map(|v| v - 10.0).collect();
        assert_eq!(speech_runs(&low), vec![(2.0, 4.0)]);
        assert!(speech_runs(&[-30.0; 4]).is_empty());
    }

    #[test]
    fn score_do_fallback_fica_abaixo_de_uma_cena_boa() {
        let a = Analysis { duration: 60.0, scenes: vec![10.0, 20.0, 30.0], audio_rms: vec![], speech: vec![] };
        let w = Window { start: 0.0, end: 45.0, scenes: 3, quality: 0.0 };
        let s = score(&a, &w);
        assert!(s > 0.0 && s.is_finite() && s < 3.5);
        let boa = Window { start: 0.0, end: 45.0, scenes: 3, quality: 7.2 };
        assert_eq!(score(&a, &boa), 7.2);
    }

    // ----------------------------------------------------- embalagem

    #[test]
    fn gancho_pega_a_primeira_sentenca_que_cabe() {
        let d = "No Império Solis, preso em um inverno eterno, apenas o casamento entre uma princesa de puro sangue real e o Dragão do Abismo pode trazer de volta o calor do sol. A Princesa Diana cresceu feliz ao lado de seus três irmãos — até a chegada de Lorena. Enganados, os irmãos a rejeitaram.";
        assert_eq!(hook_line(d).as_deref(), Some("Enganados, os irmãos a rejeitaram."));
    }

    #[test]
    fn gancho_corta_na_virgula_quando_nada_cabe() {
        let d = "Rejeitada pelos três alfas que juraram protegê-la, ela foi reivindicada pelo rei lycan que todos temiam e ninguém jamais ousou desafiar em batalha.";
        let h = hook_line(d).unwrap();
        assert!(h.ends_with('…'), "{h}");
        assert!(h.chars().count() <= HOOK_MAX_CHARS, "{h}");
        assert!(h.starts_with("Rejeitada pelos três alfas"), "{h}");
    }

    #[test]
    fn gancho_vazio_sem_sinopse() {
        assert_eq!(hook_line("   "), None);
        assert_eq!(hook_line("Curta."), None);
    }

    #[test]
    fn quebra_de_linha_por_palavras() {
        assert_eq!(wrap("A Rainha Dragão Que Eles Perderam", 20), vec!["A Rainha Dragão Que", "Eles Perderam"]);
        assert_eq!(wrap("Supercalifragilistico", 5), vec!["Supercalifragilistico"]);
        let b = fit_text("Uma Duas Tres Quatro Cinco Seis Sete Oito Nove Dez", &[(72, 10), (60, 12)], 2);
        assert_eq!(b.size, 60);
        assert_eq!(b.lines.len(), 2);
        assert!(b.lines[1].ends_with('…'));
    }

    #[test]
    fn caminho_escapado_para_o_grafo() {
        let p = Path::new(r"C:\Users\x\AppData\Local\Temp\a.ttf");
        assert_eq!(ff_path(p), "C\\\\:/Users/x/AppData/Local/Temp/a.ttf");
    }

    #[test]
    fn regiao_do_remover_reduz_e_mantem_a_caixa() {
        let b = PatchBox { t0: 0.0, t1: 1.0, x: 88, y: 136, w: 144, h: 144, cx: 160, cy: 208 };
        let r = remove_region(&b);
        assert!(r.rx < b.x && r.ry < b.y && r.rw > b.w && r.rh > b.h, "recorte com contexto: {r:?}");
        assert!(r.sw.max(r.sh) <= REMOVE_LORES_SIDE + 1 && r.sw % 2 == 0 && r.sh % 2 == 0, "{r:?}");
        let pgm = mask_pgm(&r);
        let header = format!("P5\n{} {}\n255\n", r.sw, r.sh);
        assert!(pgm.starts_with(header.as_bytes()));
        assert_eq!(pgm.len() - header.len(), (r.sw * r.sh) as usize);
        assert!(pgm[header.len()..].iter().any(|&v| v == 255) && pgm[header.len()..].iter().any(|&v| v == 0));
    }

    #[test]
    fn data_compacta() {
        assert_eq!(compact_date("2026-09-11"), "20260911");
    }

    #[test]
    fn nome_do_clipe() {
        assert_eq!(
            clip_filename("A Duquesa Rejeitada", "2026-09-11", 3),
            "A Duquesa Rejeitada - CL-20260911-03.mp4"
        );
    }

    // ----------------------------------------------------- marca de origem

    fn hit(t: f64, cx: f64, cy: f64) -> Hit {
        // Caixa fixa de ~0.11x0.06 centrada em (cx, cy).
        Hit { t, x: cx - 0.055, y: cy - 0.03, w: 0.11, h: 0.06 }
    }

    #[test]
    fn source_mark_parse() {
        assert_eq!(SourceMark::parse("cobrir"), SourceMark::Cobrir);
        assert_eq!(SourceMark::parse("manter"), SourceMark::Manter);
        assert_eq!(SourceMark::parse("remover"), SourceMark::Remover);
        // Vazio/desconhecido cai no padrao.
        assert_eq!(SourceMark::parse(""), SourceMark::Remover);
        assert_eq!(SourceMark::parse("qualquer"), SourceMark::Remover);
    }

    #[test]
    fn perfil_reelshort_carrega_e_referencial_existe() {
        let p = profiles();
        let r = p.get("reelshort").expect("perfil reelshort");
        assert!(reference_png(&r.reference).is_some());
        assert!(!r.scales.is_empty() && r.threshold > 0.0);
        // Chave de documentacao nao vira perfil.
        assert!(p.get("_nota").is_none());
    }

    fn assets(watermark: bool, badge: bool, packaging: bool, masks: usize) -> GraphAssets {
        let mut idx = 0usize;
        let mut next = || {
            idx += 1;
            idx
        };
        let watermark_idx = if watermark && !badge { Some(next()) } else { None };
        let badge_idx = if badge { Some(next()) } else { None };
        let logo_idx = if packaging { Some(next()) } else { None };
        GraphAssets {
            watermark_idx,
            badge_idx,
            logo_idx,
            font_bold: "BOLD".into(),
            font_semi: "SEMI".into(),
            masks: (0..masks).map(|i| format!("MASK{i}")).collect(),
            texts: packaging.then(|| GraphTexts {
                kicker: Some("KICK".into()),
                title: "TITLE".into(),
                title_size: 72,
                title_lines: 2,
                hook: Some("HOOK".into()),
                hook_size: 44,
                hook_lines: 1,
                card_title: "CTITLE".into(),
                card_title_size: 60,
                card_title_lines: 2,
                card_sub: Some("CSUB".into()),
                brand: "BRAND".into(),
            }),
        }
    }

    fn win() -> Window {
        Window { start: 0.0, end: 45.0, scenes: 0, quality: 0.0 }
    }

    const AUDIO_45: &str = "[0:a]loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000,afade=t=in:st=0:d=0.08,afade=t=out:st=44.92:d=0.08[a]";

    /// Sem caixas (origem sem perfil, `manter`, ou nada detectado) o grafo nao
    /// tem removelogo nem selo, em qualquer estrategia — e e o MESMO grafo.
    #[test]
    fn filtro_sem_marcas_nao_toca_na_imagem() {
        let ref_f = build_filter(SourceMark::Manter, &[], &assets(true, false, true, 0), 45.0);
        for strat in [SourceMark::Remover, SourceMark::Cobrir, SourceMark::Manter] {
            let f = build_filter(strat, &[], &assets(true, false, true, 0), 45.0);
            assert_eq!(f, ref_f, "{strat:?}");
            assert!(!f.contains("removelogo") && !f.contains("bd0"), "{strat:?}: {f}");
        }
        // Sem marca nem embalagem: so a base, o audio e o [v].
        let f = build_filter(SourceMark::Manter, &[], &assets(false, false, false, 0), 45.0);
        assert_eq!(f, format!("{BASE_GRAPH}[b0];[b0]null[v];{AUDIO_45}"));
    }

    #[test]
    fn remover_interpola_por_segmento_em_baixa_resolucao() {
        let tl = vec![
            MarkSegment { t0: 1.0, t1: 3.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 },
            MarkSegment { t0: 8.0, t1: 12.0, x: 0.8, y: 0.85, w: 0.11, h: 0.06 },
        ];
        let boxes = patch_boxes(&tl, &win());
        let f = build_filter(SourceMark::Remover, &boxes, &assets(true, false, false, 2), 45.0);
        // Um removelogo por caixa, cada um com a sua mascara, gateado no tempo.
        assert_eq!(f.matches("removelogo=f=MASK").count(), 2, "{f}");
        assert!(f.contains("removelogo=f=MASK0") && f.contains("removelogo=f=MASK1"));
        assert_eq!(f.matches(":enable='between(t,").count(), 2);
        assert!(f.contains("enable='between(t,1.000,3.000)'"));
        // Feather: alphamerge de cada patch antes do overlay.
        assert_eq!(f.matches("alphamerge").count(), 2);
        // Nossa marca no topo, por cima de tudo.
        assert!(f.contains("[1:v]overlay=(W-w)/2:H*0.115[v]"), "{f}");
    }

    #[test]
    fn remover_sem_nossa_marca_nao_referencia_input_de_marca() {
        let tl = vec![MarkSegment { t0: 1.0, t1: 3.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 }];
        let boxes = patch_boxes(&tl, &win());
        let f = build_filter(SourceMark::Remover, &boxes, &assets(false, false, false, 1), 45.0);
        assert!(!f.contains("[1:v]"), "sem marca nao ha segundo input: {f}");
        assert!(f.contains("null[v]"));
        assert_eq!(f.matches("removelogo").count(), 1);
    }

    #[test]
    fn cobrir_um_selo_por_segmento_com_janela_sobreposta() {
        let tl = vec![
            MarkSegment { t0: 1.0, t1: 3.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 },
            MarkSegment { t0: 8.0, t1: 12.0, x: 0.8, y: 0.85, w: 0.11, h: 0.06 },
        ];
        let boxes = patch_boxes(&tl, &win());
        // O selo e o input 1 e a marca fixa fica de fora (um so elemento nosso).
        let f = build_filter(SourceMark::Cobrir, &boxes, &assets(true, true, false, 0), 45.0);
        assert!(f.contains("[1:v]scale="), "logo escalada do input 1: {f}");
        assert!(!f.contains("H*0.115"), "sem marca fixa junto do selo: {f}");
        assert!(f.contains("split=2"), "2 copias do selo: {f}");
        assert_eq!(f.matches(":enable='between(t,").count(), 2, "um gate por segmento: {f}");
        assert!(f.contains("enable='between(t,0.600,3.400)'"), "padding do 1o segmento: {f}");
        assert!(f.contains("overlay=") && !f.contains("overlay=x='"), "posicao em px: {f}");
    }

    #[test]
    fn cobrir_selo_preso_ao_canvas() {
        let tl = vec![MarkSegment { t0: 1.0, t1: 3.0, x: 0.93, y: 0.95, w: 0.11, h: 0.06 }];
        let boxes = patch_boxes(&tl, &win());
        let side = badge_side(&boxes);
        let (x, y) = badge_xy(&boxes[0], side);
        assert!(x >= 0 && x + side <= 1080, "x dentro do canvas: x={x} side={side}");
        assert!(y >= 0 && y + side <= 1920, "y dentro do canvas: y={y} side={side}");
    }

    #[test]
    fn embalagem_gancho_e_cartao_no_grafo() {
        let f = build_filter(SourceMark::Manter, &[], &assets(true, false, true, 0), 45.0);
        // Gancho: kicker em caixa rosa, titulo em ExtraBold, frase em SemiBold; some ate 4s.
        assert!(f.contains("textfile=KICK") && f.contains("box=1:boxcolor=0xD6335C@0.92"), "{f}");
        assert!(f.contains("fontfile=BOLD:textfile=TITLE:fontsize=72"), "{f}");
        assert!(f.contains("fontfile=SEMI:textfile=HOOK:fontsize=44"), "{f}");
        assert!(f.contains(&hook_alpha()), "{f}");
        // Cartao: escurece a partir de 45-2.6=42.4, flor do input 2, textos e marca.
        assert!(f.contains("fade=t=in:st=42.400:d=0.45:alpha=1,colorchannelmixer=aa=0.72"), "{f}");
        assert!(f.contains("[2:v]scale=300:300"), "{f}");
        assert!(f.contains("textfile=CTITLE") && f.contains("textfile=CSUB") && f.contains("textfile=BRAND"), "{f}");
        assert!(f.contains(&card_alpha(42.4)), "{f}");
        // A marca fixa fica por cima de tudo, no topo.
        assert!(f.ends_with(&format!("[1:v]overlay=(W-w)/2:H*0.115[v];{AUDIO_45}")), "{f}");
    }

    #[test]
    fn embalagem_sem_episodio_nem_gancho() {
        let mut a = assets(false, false, true, 0);
        let t = a.texts.as_mut().unwrap();
        t.kicker = None;
        t.hook = None;
        t.card_sub = None;
        let f = build_filter(SourceMark::Manter, &[], &a, 45.0);
        assert!(!f.contains("KICK") && !f.contains("HOOK") && !f.contains("CSUB"), "{f}");
        assert!(f.contains("TITLE") && f.contains("BRAND"));
    }

    #[test]
    fn meta_do_clipe_sem_manifesto_usa_a_pasta() {
        let dir = std::env::temp_dir().join(format!("tkdrama-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Minha Novela")).unwrap();
        let m = clip_meta(&dir.join("Minha Novela"), "Minha Novela - E03.mp4");
        assert_eq!(m.title, "Minha Novela");
        assert_eq!(m.episode, None);
        assert_eq!(m.hook, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selo_cobre_o_maior_segmento() {
        let w = win();
        let tl = vec![
            MarkSegment { t0: 1.0, t1: 3.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 },
            MarkSegment { t0: 8.0, t1: 12.0, x: 0.8, y: 0.83, w: 0.16, h: 0.09 },
        ];
        let boxes = patch_boxes(&tl, &w);
        let side = badge_side(&boxes);
        // Cobre o maior segmento com o transbordo de COVER_SCALE.
        let maior = boxes.iter().map(|b| b.w.max(b.h)).max().unwrap() as f64;
        assert_eq!(side, (maior * COVER_SCALE).round().clamp(40.0, 1080.0) as i64);
        assert!(side as f64 >= maior, "selo cobre o segmento inteiro");
    }

    #[test]
    fn patch_boxes_desloca_por_window_start() {
        let w = Window { start: 8.0, end: 53.0, scenes: 0, quality: 0.0 };
        let tl = vec![
            // Antes da janela: some.
            MarkSegment { t0: 0.0, t1: 5.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 },
            // Dentro: desloca por 8s.
            MarkSegment { t0: 10.0, t1: 20.0, x: 0.1, y: 0.1, w: 0.11, h: 0.06 },
        ];
        let boxes = patch_boxes(&tl, &w);
        assert_eq!(boxes.len(), 1);
        assert!((boxes[0].t0 - 2.0).abs() < 1e-6);
        assert!((boxes[0].t1 - 12.0).abs() < 1e-6);
    }

    #[test]
    fn fusao_de_segmentos() {
        // Duas corridas em cantos opostos + um pico isolado no meio.
        let mut hits = Vec::new();
        for k in 0..3 {
            hits.push(hit(k as f64 * 0.5, 0.16, 0.10)); // canto superior-esquerdo
        }
        hits.push(hit(5.0, 0.5, 0.5)); // pico isolado -> deve cair
        for k in 0..3 {
            hits.push(hit(7.0 + k as f64 * 0.5, 0.84, 0.90)); // canto inferior-direito
        }
        let segs = group_segments(&hits, 20.0);
        assert_eq!(segs.len(), 2, "dois cantos, o pico isolado descartado");
        // Primeiro segmento no topo-esquerda, estendido nas pontas.
        assert!(segs[0].t0 <= 0.0 && segs[0].t1 >= 1.0);
        assert!(segs[0].x < 0.3 && segs[1].x > 0.5);
    }

    #[test]
    fn fusao_faz_ponte_sobre_queda_curta() {
        // Amostra some em t=1.0 (fundo claro) mas volta no mesmo lugar: um segmento.
        let hits = vec![
            hit(0.0, 0.16, 0.10),
            hit(0.5, 0.16, 0.10),
            // (buraco em 1.0)
            hit(1.5, 0.16, 0.10),
            hit(2.0, 0.16, 0.10),
        ];
        let segs = group_segments(&hits, 20.0);
        assert_eq!(segs.len(), 1);
    }
}
