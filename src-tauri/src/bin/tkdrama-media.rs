use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use tkdrama_app_lib::{cortes, download, manifest, media, reelshort};

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Validation {
    folder: String,
    mp4: usize,
    thumbs: usize,
    /// `trailer.mp4` presente e validado. Opcional: falso nao e erro.
    trailer: bool,
    total_duration_sec: f64,
    all_h264_aac: bool,
    all_faststart: bool,
    all_paths_exist: bool,
    thumbnails_distinct: bool,
    leftovers: Vec<String>,
    errors: Vec<String>,
}

fn folders(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let has_video = std::fs::read_dir(path)?.flatten().any(|e| {
        e.path().extension().is_some_and(|x| ["mp4", "ts", "m2ts"].iter().any(|v| x.eq_ignore_ascii_case(v)))
    });
    if has_video { return Ok(vec![path.to_path_buf()]); }
    let mut out: Vec<_> = std::fs::read_dir(path)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    Ok(out)
}

async fn backfill(path: &Path) -> anyhow::Result<()> {
    for dir in folders(path)? {
        let report = manifest::backfill_local(&dir).await?;
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "folder": dir, "converted": report.converted, "episodes": report.episodes,
            "thumbs": report.thumbs, "errors": report.errors,
        }))?);
    }
    Ok(())
}

async fn validate(dir: &Path) -> anyhow::Result<Validation> {
    let m = manifest::read(dir);
    let mut r = Validation {
        folder: dir.display().to_string(), all_h264_aac: true, all_faststart: true,
        all_paths_exist: true, thumbnails_distinct: true, ..Default::default()
    };
    let mut thumb_bytes: Vec<Vec<u8>> = Vec::new();
    for (number, ep) in &m.episodes {
        let video = dir.join(&ep.file);
        if !video.is_file() { r.all_paths_exist = false; r.errors.push(format!("E{number:02}: vídeo ausente")); continue; }
        match download::validate_video(&video, true).await {
            Ok(info) => {
                r.mp4 += 1; r.total_duration_sec += info.duration;
                r.all_h264_aac &= info.codec == "h264" && info.audio_codec == "aac";
                r.all_faststart &= info.faststart;
            }
            Err(e) => r.errors.push(format!("E{number:02}: {e}")),
        }
        let Some(thumb) = &ep.thumb else { r.errors.push(format!("E{number:02}: sem thumbnail")); continue };
        let thumb = dir.join(thumb.replace('/', std::path::MAIN_SEPARATOR_STR));
        if download::validate_jpeg(&thumb).await.is_err() { r.all_paths_exist = false; r.errors.push(format!("E{number:02}: thumbnail inválida")); }
        else { r.thumbs += 1; if let Ok(bytes) = std::fs::read(thumb) { if thumb_bytes.contains(&bytes) { r.thumbnails_distinct = false; } thumb_bytes.push(bytes); } }
    }
    for entry in walkdir_one_level(dir)? {
        let n = entry.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        if n.ends_with(".ts") || n.contains(".parcial.") || n.ends_with(".part") || n.ends_with(".tmp") { r.leftovers.push(entry.display().to_string()); }
    }
    if !r.leftovers.is_empty() { r.errors.push("há temporários/TS remanescentes".into()); }
    if r.mp4 != m.episodes.len() { r.errors.push("total do manifesto diverge dos MP4 válidos".into()); }

    // Trailer: opcional, mas se declarado tem de existir e ser MP4 H.264/AAC
    // com faststart, igual a um episodio.
    match &m.trailer {
        Some(rel) => {
            let caminho = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            match download::validate_video(&caminho, true).await {
                Ok(_) => r.trailer = true,
                Err(e) => { r.all_paths_exist = false; r.errors.push(format!("trailer: {e}")); }
            }
        }
        // Arquivo no disco que o manifesto nao declara e inconsistencia: quem
        // le o manifesto nunca saberia que ele existe.
        None if download::validate_video(&dir.join(manifest::TRAILER_FILE), false).await.is_ok() => {
            r.errors.push("trailer.mp4 existe no disco mas não está no manifesto".into());
        }
        None => {}
    }

    // `totalEpisodes` e a contagem da origem. Divergir dela significa pacote
    // incompleto — foi assim que "72 declarados, 71 no disco" apareceu.
    if m.total_episodes as usize != m.episodes.len() {
        r.errors.push(format!(
            "totalEpisodes={} mas o manifesto tem {} episódios",
            m.total_episodes, m.episodes.len()
        ));
    }
    let faltando: Vec<u32> = (1..=m.total_episodes).filter(|n| !m.episodes.contains_key(n)).collect();
    if !faltando.is_empty() {
        r.errors.push(format!("episódios faltando: {faltando:?}"));
    }
    Ok(r)
}

fn walkdir_one_level(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        out.push(e.path());
        if e.path().is_dir() { out.extend(std::fs::read_dir(e.path())?.flatten().map(|x| x.path())); }
    }
    Ok(out)
}

async fn download_reelshort(url: &str, root: &Path, count: usize) -> anyhow::Result<PathBuf> {
    let (lang, book) = reelshort::parse_url(url).ok_or_else(|| anyhow::anyhow!("a CLI aceita URL do ReelShort; TikTok usa a sessão da interface"))?;
    let client = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(20)).read_timeout(std::time::Duration::from_secs(45)).build()?;
    let session = reelshort::Session::default();
    let series = reelshort::fetch_series(&client, &lang, &book, &session, None, false).await?;
    let dir = media::series_dir(&root.display().to_string(), &series.name);
    std::fs::create_dir_all(&dir)?;
    let poster = manifest::fetch_poster(&client, &series, &dir, "", &[]).await.ok();
    let mut m = manifest::read(&dir);
    manifest::patch_series(&mut m, &series, poster);
    let control = Arc::new(AtomicU8::new(download::RUN));
    // Trailer antes dos episodios: e o arquivo mais leve e o que completa a
    // pasta para publicacao. Falhar aqui nao impede o resto.
    if let Ok(Some(t)) = manifest::ensure_trailer(&client, &series, &dir, &session, "", control.clone()).await {
        m.trailer = Some(t);
        manifest::write(&dir, &m)?;
    }
    for ep in series.episodes.iter().filter(|e| e.available()).take(count) {
        let (segments, _) = reelshort::resolve_media(&client, &lang, &book, ep.number, &ep.item_id, ep.locked, &session).await?;
        let file = format!("{} - E{:02}.mp4", media::safe_name(&series.name), ep.number);
        let dest = dir.join(&file);
        download::download_hls_mp4(&client, &segments, &dest, "", control.clone(), |_, _| {}).await?;
        let info = download::validate_video(&dest, true).await?;
        let thumb = manifest::ensure_video_thumb(&dest, &dir, ep.number).await?;
        manifest::patch_episode(&mut m, ep.number, manifest::validated_episode_entry(Some(ep), file, Some(thumb), &info));
        manifest::write(&dir, &m)?;
    }
    m.total_duration_sec = Some(m.episodes.values().filter_map(|e| e.duration).sum());
    manifest::write(&dir, &m)?;
    Ok(dir)
}

/// Le `dest_dir` e o interruptor da marca do `state.json` do app.
///
/// A CLI nao tem o contexto do Tauri, mas o identifier e fixo
/// (`com.tkdrama.app`), entao o mesmo estado que a interface grava e legivel
/// aqui. Leve por `Value` para nao acoplar ao struct `Settings`.
fn cortes_defaults() -> (Option<PathBuf>, bool, String, bool) {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return (None, true, "remover".into(), true);
    };
    let path = PathBuf::from(appdata).join("com.tkdrama.app").join("state.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, true, "remover".into(), true);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, true, "remover".into(), true);
    };
    let s = &v["settings"];
    let root = s["dest_dir"].as_str().map(PathBuf::from);
    // Ausente = ligado (padrao do produto).
    let marca = s["cortes_watermark"].as_bool().unwrap_or(true);
    let source_mark = s["cortes_source_mark"].as_str().unwrap_or("remover").to_string();
    let embalagem = s["cortes_embalagem"].as_bool().unwrap_or(true);
    (root, marca, source_mark, embalagem)
}

/// Escolhe a estrategia da marca de origem: `--marca-origem <remover|cobrir|manter>`
/// se dado, senao o padrao do app.
fn source_mark_arg(args: &[String], padrao: &str) -> cortes::SourceMark {
    let escolhido = args
        .iter()
        .position(|a| a == "--marca-origem")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or(padrao);
    cortes::SourceMark::parse(escolhido)
}

async fn cortes_cmd(args: &[String]) -> anyhow::Result<()> {
    let sem_marca = args.iter().any(|a| a == "--sem-marca");
    let sem_embalagem = args.iter().any(|a| a == "--sem-embalagem");
    let (root_padrao, marca_padrao, source_mark_padrao, embalagem_padrao) = cortes_defaults();
    let marca = marca_padrao && !sem_marca;
    let embalagem = embalagem_padrao && !sem_embalagem;
    let source_mark = source_mark_arg(args, &source_mark_padrao);
    // `--saida <arquivo>`: renderiza para um caminho explicito SEM registrar no
    // indice da novela — QA de render sem "queimar" o episodio do pool do lote.
    let saida = args
        .iter()
        .position(|a| a == "--saida")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    // Valores consumidos por flags nao contam como caminho solto.
    let consumido = |i: usize| {
        matches!(
            args.get(i.wrapping_sub(1)).map(String::as_str),
            Some("--n") | Some("--marca-origem") | Some("--saida")
        )
    };
    match args.first().map(String::as_str) {
        Some("um") => {
            let arquivo = args
                .iter()
                .enumerate()
                .skip(1)
                .find(|(i, a)| !a.starts_with("--") && !consumido(*i))
                .map(|(_, a)| a)
                .ok_or_else(|| anyhow::anyhow!("uso: tkdrama-media cortes um <arquivo> [--saida <mp4>] [--sem-marca] [--sem-embalagem] [--marca-origem <remover|cobrir|manter>]"))?;
            let opts = cortes::ClipOptions { watermark: marca, source_mark, packaging: embalagem };
            match saida {
                Some(dest) => {
                    let entry = cortes::make_clip_to(Path::new(arquivo), &dest, &opts).await?;
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                }
                None => {
                    let entry = cortes::make_clip(Path::new(arquivo), &opts).await?;
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                }
            }
            Ok(())
        }
        Some("marcas") => {
            // QA da deteccao: roda (ou --forca) e imprime a linha do tempo.
            let arquivo = args
                .iter()
                .skip(1)
                .find(|a| !a.starts_with("--"))
                .ok_or_else(|| anyhow::anyhow!("uso: tkdrama-media cortes marcas <arquivo> [--forca]"))?;
            let force = args.iter().any(|a| a == "--forca" || a == "--forcar");
            let source = Path::new(arquivo);
            let novela_dir = source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("arquivo sem pasta"))?;
            let segments = cortes::timeline_for(source, novela_dir, force).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "arquivo": arquivo,
                    "segmentos": segments,
                }))?
            );
            Ok(())
        }
        Some("lote") => {
            // `--n <N>` para a cota; um caminho solto sobrescreve a raiz padrao.
            let n = args
                .iter()
                .position(|a| a == "--n")
                .and_then(|i| args.get(i + 1))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(10);
            // Um caminho solto sobrescreve a raiz; ignora flags e o valor que cada
            // uma consome (`--n <N>`, `--marca-origem <v>`), para nao confundir
            // "remover"/"cobrir"/"manter" com a raiz da biblioteca.
            let root = args
                .iter()
                .enumerate()
                .skip(1)
                .find(|(i, a)| !a.starts_with("--") && a.parse::<usize>().is_err() && !consumido(*i))
                .map(|(_, a)| PathBuf::from(a))
                .or(root_padrao)
                .ok_or_else(|| anyhow::anyhow!("sem raiz da biblioteca: passe o caminho ou configure o app"))?;
            let opts = cortes::ClipOptions { watermark: marca, source_mark, packaging: embalagem };
            let report = cortes::run_lote(&root, n, &opts).await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        _ => anyhow::bail!("uso: tkdrama-media cortes <um|lote|marcas> ..."),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("cortes") => cortes_cmd(&args[2..]).await,
        Some("backfill") => backfill(Path::new(args.get(2).ok_or_else(|| anyhow::anyhow!("uso: tkdrama-media backfill <pasta>"))?)).await,
        Some("validate") => {
            let report = validate(Path::new(args.get(2).ok_or_else(|| anyhow::anyhow!("uso: tkdrama-media validate <pasta-da-novela>"))?)).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.errors.is_empty() { Ok(()) } else { anyhow::bail!("validação falhou") }
        }
        Some("download") => {
            let url = args.get(2).ok_or_else(|| anyhow::anyhow!("uso: tkdrama-media download <url-reelshort> <destino> [quantidade]"))?;
            let root = Path::new(args.get(3).ok_or_else(|| anyhow::anyhow!("destino ausente"))?);
            let count = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1);
            println!("{}", download_reelshort(url, root, count).await?.display());
            Ok(())
        }
        _ => anyhow::bail!("comandos: download, backfill, validate, cortes"),
    }
}
