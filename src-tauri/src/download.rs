//! Download de um episodio: retomavel, com fallback entre CDNs e pausa/cancelamento.
//!
//! Porte do nucleo ja validado em tkdrama.py (40/40 episodios integros), com o
//! acrescimo de controle de pausa/cancelamento exigido pela fila do app.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::media::{Gear, UA};

pub const RUN: u8 = 0;
pub const PAUSE: u8 = 1;
pub const CANCEL: u8 = 2;

/// Sinalizador compartilhado com a fila para pausar/cancelar em andamento.
pub type Control = Arc<AtomicU8>;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Done(u64),
    Paused,
    Canceled,
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// MPEG-TS intermediario do ReelShort. Usar o nome final com outra extensao
/// permite reaproveitar os `.ts` produzidos por versoes antigas sem baixa-los
/// outra vez.
pub fn hls_source_path(dest: &Path) -> PathBuf {
    dest.with_extension("ts")
}

/// Saida privada do remux. O `.mp4` publico so aparece por rename depois de o
/// FFmpeg terminar com sucesso, portanto nenhum leitor enxerga arquivo parcial.
pub fn video_part_path(dest: &Path) -> PathBuf {
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
    dest.with_file_name(format!("{stem}.parcial.mp4"))
}

pub fn nonempty(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

/// Localiza o sidecar que o Tauri copia ao lado do executavel. O segundo
/// candidato cobre os binarios de teste, que rodam em `target/*/deps`.
pub fn ffmpeg_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("nao consegui localizar o executavel do app")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("o executavel do app nao tem pasta"))?;
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    for candidate in [dir.join(name), dir.join("..").join(name)] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("FFmpeg nao foi encontrado junto do aplicativo; reinstale o tkdrama")
}

/// Metadados medidos no arquivo final, nunca confiados apenas à API.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub audio_codec: String,
    pub container: String,
    pub faststart: bool,
}

fn parse_duration(text: &str) -> Option<f64> {
    let raw = text.split("Duration:").nth(1)?.split(',').next()?.trim();
    let mut p = raw.split(':');
    let h = p.next()?.parse::<f64>().ok()?;
    let m = p.next()?.parse::<f64>().ok()?;
    let s = p.next()?.parse::<f64>().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

fn stream_codec(line: &str, kind: &str) -> Option<String> {
    let marker = format!("{kind}: ");
    let raw = line.split(&marker).nth(1)?;
    Some(raw.split([' ', ',']).next()?.trim().to_ascii_lowercase())
}

fn video_dimensions(line: &str) -> Option<(u32, u32)> {
    for token in line.split(|c: char| c == ' ' || c == ',') {
        let clean = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != 'x');
        let Some((w, h)) = clean.split_once('x') else { continue };
        let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) else { continue };
        if w >= 16 && h >= 16 {
            return Some((w, h));
        }
    }
    None
}

/// Confirma que `moov` aparece antes de `mdat`, condição objetiva de faststart.
fn has_faststart(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut at = 0u64;
    let mut moov = None;
    let mut mdat = None;
    while at + 8 <= len {
        file.seek(SeekFrom::Start(at))?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = head[4..8].try_into().unwrap();
        let header = if size == 1 {
            file.read_exact(&mut head[8..16])?;
            size = u64::from_be_bytes(head[8..16].try_into().unwrap());
            16
        } else { 8 };
        if typ == *b"moov" { moov = Some(at); }
        if typ == *b"mdat" { mdat = Some(at); }
        if moov.is_some() && mdat.is_some() { break; }
        if size == 0 { break; }
        if size < header || at.saturating_add(size) > len { bail!("estrutura MP4 truncada"); }
        at += size;
    }
    Ok(matches!((moov, mdat), (Some(moov), Some(mdat)) if moov < mdat))
}

/// Checagem sincrona e barata para reconciliar a fila com o disco. Confirma o
/// contêiner e percorre apenas os cabeçalhos dos atoms; nunca carrega o vídeo
/// inteiro na memória.
pub fn published_mp4(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let cap = file.metadata().map(|m| m.len().min(4 * 1024 * 1024) as usize).unwrap_or(0);
    let mut head = vec![0u8; cap];
    let Ok(read) = file.read(&mut head) else { return false };
    let bytes = &head[..read];
    bytes.windows(4).any(|w| w == b"ftyp")
        && bytes.windows(4).any(|w| w == b"avc1" || w == b"avc3")
        && bytes.windows(4).any(|w| w == b"mp4a")
        && has_faststart(path).unwrap_or(false)
}

async fn ffmpeg_output(args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(ffmpeg_path()?);
    cmd.kill_on_drop(true).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x0800_0000);
    }
    cmd.output().await.context("não consegui executar o FFmpeg empacotado")
}

/// Inspeciona streams, duração, dimensões, contêiner e posição do `moov`.
pub async fn probe_video(path: &Path) -> Result<VideoInfo> {
    if !nonempty(path) { bail!("arquivo ausente ou vazio") }
    let args: [&std::ffi::OsStr; 5] = [
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-i"),
        path.as_os_str(),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("null"),
    ];
    let out = ffmpeg_output(&args).await?;
    let text = String::from_utf8_lossy(&out.stderr);
    let duration = parse_duration(&text).unwrap_or(0.0);
    let video_line = text.lines().find(|l| l.contains(" Video: "))
        .ok_or_else(|| anyhow!("arquivo sem vídeo"))?;
    let audio_line = text.lines().find(|l| l.contains(" Audio: "))
        .ok_or_else(|| anyhow!("arquivo sem áudio"))?;
    let codec = stream_codec(video_line, "Video").unwrap_or_default();
    let audio_codec = stream_codec(audio_line, "Audio").unwrap_or_default();
    let (width, height) = video_dimensions(video_line).unwrap_or_default();
    let container = if published_mp4(path) { "mp4" } else { "unknown" };
    Ok(VideoInfo {
        duration, width, height, codec, audio_codec,
        container: container.into(),
        faststart: container == "mp4" && has_faststart(path)?,
    })
}

/// Validação de publicação. Com `deep`, decodifica o episódio inteiro para
/// detectar truncamento e erros que uma simples leitura de cabeçalho não vê.
pub async fn validate_video(path: &Path, deep: bool) -> Result<VideoInfo> {
    let info = probe_video(path).await?;
    if info.container != "mp4" { bail!("o arquivo não é MP4 real") }
    if info.codec != "h264" { bail!("vídeo incompatível: {}", info.codec) }
    if info.audio_codec != "aac" { bail!("áudio incompatível: {}", info.audio_codec) }
    if info.duration <= 0.0 || info.width == 0 || info.height == 0 { bail!("metadados de vídeo inválidos") }
    if !info.faststart { bail!("MP4 sem faststart") }
    if deep {
        let sink = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let args: [&std::ffi::OsStr; 11] = [
            std::ffi::OsStr::new("-v"), std::ffi::OsStr::new("error"),
            std::ffi::OsStr::new("-xerror"), std::ffi::OsStr::new("-i"), path.as_os_str(),
            std::ffi::OsStr::new("-map"), std::ffi::OsStr::new("0:v:0"),
            std::ffi::OsStr::new("-map"), std::ffi::OsStr::new("0:a:0"),
            std::ffi::OsStr::new("-f"), std::ffi::OsStr::new("null"),
        ];
        let mut args = args.to_vec();
        args.push(std::ffi::OsStr::new(sink));
        let out = ffmpeg_output(&args).await?;
        if !out.status.success() {
            bail!("MP4 corrompido: {}", String::from_utf8_lossy(&out.stderr).trim())
        }
    }
    Ok(info)
}

/// Normaliza qualquer entrada suportada para H.264/AAC MP4, validando antes
/// da publicação. A entrada só é apagada depois do rename final bem-sucedido.
pub async fn prepare_video(input: &Path, dest: &Path, control: &Control) -> Result<Outcome> {
    if validate_video(dest, false).await.is_ok() {
        return Ok(Outcome::Done(std::fs::metadata(dest)?.len()));
    }
    if !nonempty(input) { bail!("arquivo de origem ausente ou vazio") }
    let source = probe_video(input).await?;
    let tmp = video_part_path(dest);
    let _ = tokio::fs::remove_file(&tmp).await;
    let copy_video = source.codec == "h264";
    let copy_audio = source.audio_codec == "aac";
    let mut cmd = tokio::process::Command::new(ffmpeg_path()?);
    cmd.kill_on_drop(true).stdout(Stdio::null()).stderr(Stdio::null())
        .arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin").arg("-y")
        .arg("-i").arg(input).arg("-map").arg("0:v:0").arg("-map").arg("0:a:0")
        .arg("-c:v").arg(if copy_video { "copy" } else { "libopenh264" });
    if !copy_video { cmd.arg("-pix_fmt").arg("yuv420p").arg("-b:v").arg("2500k"); }
    cmd
        .arg("-c:a").arg(if copy_audio { "copy" } else { "aac" })
        .arg("-movflags").arg("+faststart").arg(&tmp);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.as_std_mut().creation_flags(0x0800_0000); }
    let mut child = cmd.spawn().context("não consegui executar o FFmpeg empacotado")?;
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                if !status.success() { let _ = tokio::fs::remove_file(&tmp).await; bail!("não consegui preparar o MP4") }
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let out = match control.load(Ordering::Relaxed) { PAUSE => Some(Outcome::Paused), CANCEL => Some(Outcome::Canceled), _ => None };
                if let Some(out) = out { let _ = child.kill().await; let _ = child.wait().await; return Ok(out); }
            }
        }
    }
    validate_video(&tmp, true).await.map_err(|e| { let _ = std::fs::remove_file(&tmp); e })?;
    if dest.exists() { tokio::fs::remove_file(dest).await?; }
    tokio::fs::rename(&tmp, dest).await?;
    let size = std::fs::metadata(dest)?.len();
    if input != dest { let _ = tokio::fs::remove_file(input).await; }
    Ok(Outcome::Done(size))
}

pub async fn validate_jpeg(path: &Path) -> Result<(u32, u32)> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 512 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        bail!("JPEG ausente, truncado ou inválido")
    }
    let args: [&std::ffi::OsStr; 3] = [
        std::ffi::OsStr::new("-hide_banner"), std::ffi::OsStr::new("-i"), path.as_os_str()
    ];
    let out = ffmpeg_output(&args).await?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains(" Video: "))
        .ok_or_else(|| anyhow!("imagem sem dimensões"))?;
    video_dimensions(line).ok_or_else(|| anyhow!("imagem sem dimensões"))
}

async fn render_jpeg(input: &Path, dest: &Path, seek: Option<f64>, wide: bool) -> Result<()> {
    if validate_jpeg(dest).await.is_ok() { return Ok(()) }
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
    let tmp = dest.with_file_name(format!("{stem}.parcial.jpg"));
    let _ = tokio::fs::remove_file(&tmp).await;
    let filter = if wide {
        "scale=640:360:force_original_aspect_ratio=increase,crop=640:360"
    } else {
        "scale='min(1080,iw)':-2"
    };
    let mut cmd = tokio::process::Command::new(ffmpeg_path()?);
    cmd.kill_on_drop(true).stdout(Stdio::null()).stderr(Stdio::piped())
        .arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin").arg("-y");
    if let Some(seconds) = seek { cmd.arg("-ss").arg(format!("{seconds:.3}")); }
    cmd.arg("-i").arg(input).arg("-frames:v").arg("1").arg("-vf").arg(filter)
        .arg("-q:v").arg("2").arg(&tmp);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.as_std_mut().creation_flags(0x0800_0000); }
    let out = cmd.output().await?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("não consegui criar JPEG: {}", String::from_utf8_lossy(&out.stderr).trim())
    }
    validate_jpeg(&tmp).await.map_err(|e| { let _ = std::fs::remove_file(&tmp); e })?;
    if dest.exists() { tokio::fs::remove_file(dest).await?; }
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// Converte uma capa/miniatura remota para JPEG local validado.
pub async fn normalize_jpeg(input: &Path, dest: &Path, wide: bool) -> Result<()> {
    render_jpeg(input, dest, None, wide).await
}

/// Gera uma miniatura 16:9 em 25% do episódio (ou outra fração solicitada).
pub async fn generate_thumbnail(video: &Path, dest: &Path, fraction: f64) -> Result<()> {
    let info = validate_video(video, false).await?;
    let fraction = fraction.clamp(0.20, 0.35);
    render_jpeg(video, dest, Some(info.duration * fraction), true).await
}

/// Remuxa o TS para MP4 sem recodificar audio ou video.
///
/// O TS original so e removido depois desta funcao devolver `Done`. Em pausa,
/// cancelamento ou falha, ele continua no disco e a proxima tentativa recomeca
/// diretamente daqui, sem rede.
async fn remux_hls(input: &Path, dest: &Path, control: &Control) -> Result<Outcome> {
    prepare_video(input, dest, control).await
}

/// Um episodio ja esta pronto se existe e bate com o tamanho declarado.
///
/// Sem tamanho declarado nao da para afirmar nada: dizer "pronto" ali marcaria
/// como concluido qualquer arquivo truncado que estivesse na pasta.
pub fn already_complete(dest: &Path, expected: u64) -> bool {
    expected > 0 && std::fs::metadata(dest).map(|m| m.len()) .ok() == Some(expected)
}

/// Baixa de uma URL especifica, retomando o .part se houver.
async fn stream_one(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    expected: u64,
    cookie: &str,
    control: &Control,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<Outcome> {
    let mut have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    // .part maior que o esperado = lixo de uma tentativa anterior.
    if expected > 0 && have > expected {
        let _ = tokio::fs::remove_file(part).await;
        have = 0;
    }

    let mut req = client
        .get(url)
        .header("User-Agent", UA)
        .header("Referer", "https://www.tiktok.com/")
        .header("Accept", "*/*");
    // O CDN de video de drama e travado por sessao: sem o cookie ele responde
    // 404. O downloader que funcionou (tkdrama.py) sempre enviava os cookies.
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        // Captura a explicacao do CDN: o corpo de um 404/403 do TikTok costuma
        // dizer o motivo real (assinatura, regiao, expiracao).
        let code = status.as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(400).collect();
        bail!(
            "HTTP {code} | ct={ct} | body={} | url={url}",
            snippet.replace('\n', " ").replace('\r', " ")
        );
    }

    // Total real: do Content-Range quando e resposta parcial.
    let total = if status.as_u16() == 206 {
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|s| s.trim().parse::<u64>().ok())
    } else {
        resp.content_length().map(|c| c + have)
    };

    // URL reassinada pode apontar para outra variante: nunca emendar por cima.
    if let (Some(t), true) = (total, expected > 0) {
        if t != expected {
            let _ = tokio::fs::remove_file(part).await;
            bail!("tamanho inesperado ({t} != {expected})");
        }
    }

    let append = have > 0 && status.as_u16() == 206;
    if !append {
        have = 0;
    }

    if let Some(dir) = part.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(part)
        .await?;

    let goal = total.unwrap_or(expected);
    let mut written = have;
    let mut stream = resp.bytes_stream();

    // Espera por pedacos em fatias de 1s em vez de bloquear no stream: pausar
    // e cancelar respondem na hora (antes so surtiam efeito quando o proximo
    // pedaco chegasse, o que num CDN travado podia nunca acontecer) e uma
    // conexao muda e detectada em vez de segurar a vaga da fila para sempre.
    const STALL_SECS: u32 = 60;
    let mut idle: u32 = 0;
    loop {
        match control.load(Ordering::Relaxed) {
            PAUSE => {
                file.flush().await?;
                return Ok(Outcome::Paused);
            }
            CANCEL => {
                file.flush().await?;
                return Ok(Outcome::Canceled);
            }
            _ => {}
        }

        let bytes = match tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
        {
            Err(_) => {
                idle += 1;
                if idle >= STALL_SECS {
                    file.flush().await?;
                    bail!("conexao travou ({STALL_SECS}s sem dados)");
                }
                // Reporta mesmo parado: e assim que a velocidade na tela cai
                // para zero em vez de congelar no ultimo valor bom.
                on_progress(written, goal);
                continue;
            }
            Ok(None) => break,
            Ok(Some(chunk)) => chunk?,
        };

        idle = 0;
        file.write_all(&bytes).await?;
        written += bytes.len() as u64;
        on_progress(written, goal);
    }

    file.flush().await?;
    drop(file);

    if goal > 0 && written != goal {
        bail!("incompleto: {written}/{goal}");
    }
    Ok(Outcome::Done(written))
}

/// Onde fica a anotacao de progresso da retomada HLS.
fn index_path(dest: &Path) -> PathBuf {
    let mut s = part_path(dest).into_os_string();
    s.push(".idx");
    PathBuf::from(s)
}

/// Identifica a playlist para a qual o `.part` foi gravado.
///
/// Usa o nome dos segmentos, nao a URL inteira: se o CDN passar a assinar os
/// links, a assinatura muda a cada consulta e a retomada seria descartada a toa.
/// O nome do arquivo, esse, so muda se o video for realmente outro.
fn playlist_fingerprint(segments: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for url in segments {
        let name = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .rsplit('/')
            .next()
            .unwrap_or(url);
        for b in name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

/// Le a anotacao: (segmentos ja gravados, bytes que eles ocupam).
///
/// So vale se tres coisas baterem: o `.part` tem o tamanho anotado, a lista tem
/// o mesmo tamanho e e a MESMA lista. Sem a impressao digital, um episodio
/// reencodado do outro lado faria a retomada emendar segmentos de dois videos
/// diferentes no mesmo arquivo — sem erro nenhum, so um video corrompido.
fn read_index(dest: &Path, part_len: u64, segments: &[String]) -> (usize, u64) {
    let Ok(text) = std::fs::read_to_string(index_path(dest)) else {
        return (0, 0);
    };
    let mut parts = text.split_whitespace();
    let done: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let bytes: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let total: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let fingerprint: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    let mesma_lista =
        total == segments.len() && fingerprint == playlist_fingerprint(segments);
    if done > 0 && done < segments.len() && bytes == part_len && mesma_lista {
        (done, bytes)
    } else {
        (0, 0)
    }
}

fn write_index(dest: &Path, done: usize, bytes: u64, segments: &[String]) {
    let _ = std::fs::write(
        index_path(dest),
        format!(
            "{done} {bytes} {} {}",
            segments.len(),
            playlist_fingerprint(segments)
        ),
    );
}

/// Apaga os rastros de um download incompleto (.part e a anotacao de retomada).
pub fn remove_artifacts(dest: &Path) {
    let _ = std::fs::remove_file(part_path(dest));
    let _ = std::fs::remove_file(index_path(dest));
}

/// Baixa um episodio entregue em HLS: os segmentos sao concatenados na ordem.
///
/// Diferente do caminho do TikTok, onde `urls` sao CDNs alternativos do mesmo
/// arquivo, aqui TODOS os segmentos sao necessarios e a ordem importa. Como o
/// tamanho total so se conhece no fim, o progresso e estimado pela media do que
/// ja veio — vai ficando exato conforme avanca.
pub async fn download_hls(
    client: &reqwest::Client,
    segments: &[String],
    dest: &Path,
    cookie: &str,
    control: Control,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<Outcome> {
    if segments.is_empty() {
        bail!("episodio sem segmentos de video");
    }
    let part = part_path(dest);
    if let Some(dir) = part.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }

    let part_len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let (mut done, mut written) = read_index(dest, part_len, segments);
    if done >= segments.len() {
        // Anotacao de uma lista antiga: recomeca do zero.
        done = 0;
        written = 0;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(done > 0)
        .truncate(done == 0)
        .open(&part)
        .await?;
    if done == 0 {
        written = 0;
    }

    let total = segments.len();
    for (i, url) in segments.iter().enumerate().skip(done) {
        match control.load(Ordering::Relaxed) {
            PAUSE => {
                file.flush().await?;
                write_index(dest, i, written, segments);
                return Ok(Outcome::Paused);
            }
            CANCEL => {
                file.flush().await?;
                write_index(dest, i, written, segments);
                return Ok(Outcome::Canceled);
            }
            _ => {}
        }

        // Uma falha isolada de segmento costuma ser soluco de rede: tenta de
        // novo aqui antes de devolver o episodio inteiro para a fila.
        let mut bytes = None;
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match fetch_segment(client, url, cookie).await {
                Ok(b) => {
                    bytes = Some(b);
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(std::time::Duration::from_secs(1 + attempt)).await;
                    }
                }
            }
        }
        let Some(bytes) = bytes else {
            file.flush().await?;
            write_index(dest, i, written, segments);
            return Err(last.unwrap_or_else(|| anyhow!("segmento {} falhou", i + 1)));
        };

        file.write_all(&bytes).await?;
        written += bytes.len() as u64;
        write_index(dest, i + 1, written, segments);

        // Estimativa do total pela media dos segmentos ja baixados.
        let estimate = written / (i as u64 + 1) * total as u64;
        on_progress(written, estimate.max(written));
    }

    file.flush().await?;
    drop(file);
    tokio::fs::rename(&part, dest).await?;
    let _ = std::fs::remove_file(index_path(dest));
    on_progress(written, written);
    Ok(Outcome::Done(written))
}

/// Baixa o HLS do ReelShort e publica exclusivamente MP4.
///
/// O TS fica ao lado do destino enquanto ainda e necessario. Isso preserva a
/// retomada e tambem transforma localmente arquivos criados por versoes antigas
/// do app. Nenhum byte e baixado de novo quando esse intermediario ja existe.
pub async fn download_hls_mp4(
    client: &reqwest::Client,
    segments: &[String],
    dest: &Path,
    cookie: &str,
    control: Control,
    on_progress: impl FnMut(u64, u64),
) -> Result<Outcome> {
    if validate_video(dest, false).await.is_ok() {
        return Ok(Outcome::Done(std::fs::metadata(dest)?.len()));
    }
    let input = hls_source_path(dest);
    let downloaded = if nonempty(&input) {
        Outcome::Done(std::fs::metadata(&input)?.len())
    } else {
        // Um arquivo vazio nao e recuperavel e impediria o rename no Windows.
        let _ = tokio::fs::remove_file(&input).await;
        download_hls(client, segments, &input, cookie, control.clone(), on_progress).await?
    };

    match downloaded {
        Outcome::Done(_) => {
            let outcome = remux_hls(&input, dest, &control).await?;
            if matches!(outcome, Outcome::Done(_)) {
                let _ = tokio::fs::remove_file(&input).await;
                let _ = tokio::fs::remove_file(index_path(&input)).await;
            }
            Ok(outcome)
        }
        other => Ok(other),
    }
}

/// Baixa e junta um episodio do CandyJar: FFmpeg recebe a(s) playlist(s) HLS
/// diretamente e faz o mux, sem passar por Rust. Video e audio costumam vir
/// em faixas separadas (fMP4/CMAF); quando nao vem (conteudo antigo, ver
/// `candyjar::select_variant`), uma unica entrada carrega os dois.
///
/// Diferente do ReelShort, aqui nao ha retomada por byte: cada tentativa reinicia
/// do zero. O motivo e o formato — segmentos fMP4 dependem de um `init.mp4`
/// (`EXT-X-MAP`) que uma leitura manual de segmentos poderia descartar em
/// silencio (ver o modulo `candyjar`), entao o FFmpeg cuida da playlist
/// inteira de uma vez, e uma pausa/queda no meio perde o que foi baixado ate
/// ali. Episodios sao curtos (poucos minutos): o custo de recomecar e baixo.
pub async fn download_candyjar_mp4(
    video_url: &str,
    // `None` quando a variante ja carrega audio embutido (conteudo antigo,
    // MPEG-TS classico — ver `candyjar::select_variant`); nesse caso uma
    // entrada so basta e os dois mapas apontam para ela.
    audio_url: Option<&str>,
    dest: &Path,
    control: Control,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<Outcome> {
    if video_url.is_empty() {
        bail!("episodio sem URL de midia (bloqueado)");
    }
    if validate_video(dest, false).await.is_ok() {
        return Ok(Outcome::Done(std::fs::metadata(dest)?.len()));
    }

    let tmp = video_part_path(dest);
    let _ = tokio::fs::remove_file(&tmp).await;
    if let Some(dir) = tmp.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }

    let mut cmd = tokio::process::Command::new(ffmpeg_path()?);
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin").arg("-y")
        .arg("-i").arg(video_url);
    let audio_map = match audio_url {
        Some(url) => {
            cmd.arg("-i").arg(url);
            "1:a:0"
        }
        None => "0:a:0",
    };
    cmd.arg("-map").arg("0:v:0").arg("-map").arg(audio_map)
        .arg("-c:v").arg("copy").arg("-c:a").arg("copy")
        .arg("-movflags").arg("+faststart")
        .arg(&tmp);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().context("não consegui executar o FFmpeg empacotado")?;

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                if !status.success() {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    bail!("não consegui baixar o episódio do CandyJar");
                }
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {
                let out = match control.load(Ordering::Relaxed) {
                    PAUSE => Some(Outcome::Paused),
                    CANCEL => Some(Outcome::Canceled),
                    _ => None,
                };
                if let Some(out) = out {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(out);
                }
                let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                on_progress(size, 0);
            }
        }
    }

    validate_video(&tmp, true).await.map_err(|e| { let _ = std::fs::remove_file(&tmp); e })?;
    if dest.exists() { tokio::fs::remove_file(dest).await?; }
    tokio::fs::rename(&tmp, dest).await?;
    let size = std::fs::metadata(dest)?.len();
    on_progress(size, size);
    Ok(Outcome::Done(size))
}

/// Remove tanto a saida final quanto todos os intermediarios do ReelShort.
pub fn remove_hls_mp4_artifacts(dest: &Path) {
    let input = hls_source_path(dest);
    remove_artifacts(&input);
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(video_part_path(dest));
    remove_artifacts(dest);
}

async fn fetch_segment(client: &reqwest::Client, url: &str, cookie: &str) -> Result<Vec<u8>> {
    let mut req = client
        .get(url)
        .header("User-Agent", UA)
        .header("Referer", "https://www.reelshort.com/")
        .header("Accept", "*/*");
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("segmento respondeu HTTP {}", status.as_u16());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Baixa um episodio tentando cada CDN da lista ate um funcionar.
pub async fn download_episode(
    client: &reqwest::Client,
    gear: &Gear,
    dest: &Path,
    cookie: &str,
    control: Control,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<Outcome> {
    if gear.urls.is_empty() {
        bail!("episodio sem URL de midia (bloqueado)");
    }
    if validate_video(dest, false).await.is_ok() {
        return Ok(Outcome::Done(std::fs::metadata(dest)?.len()));
    }

    let part = part_path(dest);
    let mut last: Option<anyhow::Error> = None;

    for url in &gear.urls {
        for attempt in 0..2 {
            if control.load(Ordering::Relaxed) == CANCEL {
                return Ok(Outcome::Canceled);
            }
            match stream_one(client, url, &part, gear.size, cookie, &control, &mut on_progress).await
            {
                Ok(Outcome::Done(n)) => {
                    let _ = n;
                    return prepare_video(&part, dest, &control).await;
                }
                Ok(other) => return Ok(other), // pausa/cancelamento: preserva o .part
                Err(e) => {
                    let msg = e.to_string();
                    last = Some(e);
                    // URL morta ou expirada (403/401/404): nao adianta repetir a
                    // mesma; pula direto para o proximo CDN.
                    if msg.contains("HTTP 403")
                        || msg.contains("HTTP 401")
                        || msg.contains("HTTP 404")
                    {
                        break;
                    }
                    if attempt == 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
    }

    Err(anyhow!(
        "todas as {} URLs falharam: {}",
        gear.urls.len(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(nomes: &[&str]) -> Vec<String> {
        nomes
            .iter()
            .map(|n| format!("https://cdn.x.com/v/{n}"))
            .collect()
    }

    #[test]
    fn retomada_hls_so_vale_para_a_mesma_playlist() {
        let dir = std::env::temp_dir().join("tkdrama_test_idx");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("E01.ts");

        let lista = segs(&["a-1.ts", "a-2.ts", "a-3.ts", "a-4.ts"]);
        std::fs::write(part_path(&dest), vec![0u8; 500]).unwrap();
        write_index(&dest, 2, 500, &lista);

        // Mesma lista e .part do tamanho anotado: retoma do segmento 2.
        assert_eq!(read_index(&dest, 500, &lista), (2, 500));

        // Episodio reencodado do outro lado: segmentos com outro nome. Emendar
        // aqui geraria um video corrompido em silencio.
        let outra = segs(&["b-1.ts", "b-2.ts", "b-3.ts", "b-4.ts"]);
        assert_eq!(read_index(&dest, 500, &outra), (0, 0));

        // Lista de outro tamanho tambem invalida a retomada.
        let menor = segs(&["a-1.ts", "a-2.ts"]);
        assert_eq!(read_index(&dest, 500, &menor), (0, 0));

        // .part que nao bate com o anotado = gravacao interrompida no meio.
        assert_eq!(read_index(&dest, 499, &lista), (0, 0));

        // A URL pode ganhar assinatura sem que o video mude: isso NAO pode
        // descartar a retomada.
        let assinada: Vec<String> = lista.iter().map(|u| format!("{u}?token=abc")).collect();
        assert_eq!(read_index(&dest, 500, &assinada), (2, 500));

        remove_artifacts(&dest);
        assert!(!part_path(&dest).exists());
        assert!(!index_path(&dest).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caminho_part() {
        let p = part_path(Path::new("D:/x/E01.mp4"));
        assert_eq!(p.to_string_lossy(), "D:/x/E01.mp4.part");
    }

    #[test]
    fn completo_so_quando_tamanho_bate() {
        let dir = std::env::temp_dir().join("tkdrama_test_complete");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.mp4");
        std::fs::write(&f, vec![0u8; 100]).unwrap();

        assert!(already_complete(&f, 100));
        assert!(!already_complete(&f, 200));
        // Sem tamanho declarado nao da para afirmar que esta pronto.
        assert!(!already_complete(&f, 0));
        assert!(!already_complete(&dir.join("nao_existe.mp4"), 100));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Um `.ts` legado deve virar MP4 localmente. A lista vazia prova que o
    /// caminho nao tentou consultar nem baixar segmento algum outra vez.
    #[cfg(windows)]
    #[tokio::test]
    async fn ts_legado_e_remuxado_sem_novo_download() {
        let dir = std::env::temp_dir().join(format!(
            "tkdrama_test_remux_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("E01.mp4");
        let input = hls_source_path(&dest);

        // Gera um TS minimo e valido com o proprio sidecar. O teste seguinte
        // exercita somente remux (`copy`), que e o caminho usado em producao.
        let mut make = std::process::Command::new(ffmpeg_path().unwrap());
        use std::os::windows::process::CommandExt;
        make.creation_flags(0x0800_0000).args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=16x16:rate=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-t",
            "1",
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            "-b:v",
            "100k",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
        ]);
        assert!(make.arg(&input).status().unwrap().success());

        let outcome = download_hls_mp4(
            &reqwest::Client::new(),
            &[],
            &dest,
            "",
            Arc::new(AtomicU8::new(RUN)),
            |_, _| {},
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(n) if n > 0));
        assert!(dest.is_file());
        assert!(!input.exists(), "o TS so some depois de o MP4 ficar pronto");
        assert!(published_mp4(&dest));
        let head = std::fs::read(&dest).unwrap();
        assert_eq!(&head[4..8], b"ftyp");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codec legado não pode apenas falhar nem ser publicado como está: o
    /// fallback entrega exatamente H.264/AAC.
    #[cfg(windows)]
    #[tokio::test]
    async fn codec_incompativel_e_convertido_para_h264_aac() {
        let dir = std::env::temp_dir().join(format!("tkdrama_test_transcode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("legado.ts");
        let dest = dir.join("Novela - E01.mp4");
        let ok = std::process::Command::new(ffmpeg_path().unwrap()).args([
            "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
            "testsrc=size=64x64:rate=10", "-f", "lavfi", "-i",
            "sine=frequency=440:sample_rate=44100", "-t", "1", "-c:v", "mpeg2video",
            "-c:a", "mp2", "-f", "mpegts",
        ]).arg(&input).status().unwrap().success();
        assert!(ok);
        prepare_video(&input, &dest, &Arc::new(AtomicU8::new(RUN))).await.unwrap();
        let info = validate_video(&dest, true).await.unwrap();
        assert_eq!(info.codec, "h264");
        assert_eq!(info.audio_codec, "aac");
        assert!(info.faststart);
        assert!(!input.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caminhos_do_hls_ficam_separados_do_mp4_publico() {
        let dest = Path::new("D:/novela/E01.mp4");
        assert_eq!(hls_source_path(dest), PathBuf::from("D:/novela/E01.ts"));
        assert_eq!(part_path(&hls_source_path(dest)), PathBuf::from("D:/novela/E01.ts.part"));
        assert_eq!(video_part_path(dest), PathBuf::from("D:/novela/E01.parcial.mp4"));
    }
}
