//! Download de um episodio: retomavel, com fallback entre CDNs e pausa/cancelamento.
//!
//! Porte do nucleo ja validado em tkdrama.py (40/40 episodios integros), com o
//! acrescimo de controle de pausa/cancelamento exigido pela fila do app.

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
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
    if already_complete(dest, gear.size) {
        return Ok(Outcome::Done(gear.size));
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
                    tokio::fs::rename(&part, dest).await?;
                    return Ok(Outcome::Done(n));
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
}
