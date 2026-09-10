//! Adicionar muitas novelas de uma vez.
//!
//! O gargalo real do app nunca foi baixar: era abrir a plataforma, clicar na
//! novela, copiar o link, voltar, colar, esperar, repetir. Trinta novelas sao
//! trinta idas e voltas. Este modulo troca isso por uma colagem so.
//!
//! Tres decisoes carregam o arquivo:
//!
//! - **A entrada e texto solto, nao uma lista formatada.** O usuario cola o que
//!   tiver na mao — uma coluna de links, um bloco de conversa, HTML copiado de
//!   uma pagina — e a varredura acha os links no meio. Exigir formato seria
//!   devolver para a mao o trabalho que este modulo existe para tirar.
//!
//! - **O ritmo e o mesmo de `watch.rs`: 20s entre uma novela e outra.** Nao e
//!   cortesia. Rajada de consulta e o que derruba a sessao, e sessao morta para
//!   o app inteiro, nao so para o lote. Uma lista de 50 leva ~17 minutos de
//!   consulta — por isso a rodada e cancelavel e anuncia onde esta.
//!
//! - **Uma novela que falha nao derruba o lote.** Cada item tem o seu proprio
//!   resultado; o relatorio no fim diz quais entraram, quais nao, e por que.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use crate::media::{self, Source};
use crate::queue::{self, AppState};
use crate::{candyjar, dramabox, reelshort, tiktok, watch};

/// Espera entre uma novela e a seguinte. Igual ao `PACE` de `watch.rs`, e pelo
/// mesmo motivo: o limite nao e do app, e da plataforma do outro lado.
const PACE: Duration = Duration::from_secs(20);

const GB: u64 = 1024 * 1024 * 1024;

/// Uma rodada de lote por vez. Duas em paralelo dobrariam a taxa de consulta —
/// exatamente o que o `PACE` existe para evitar.
static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Dominios que o app sabe ler. Serve para aceitar link colado sem `https://`.
const DOMAINS: [&str; 4] = [
    "tiktok.com",
    "reelshort.com",
    "candyjar.com",
    "dramabox.com",
];

// ------------------------------------------------------------------- tipos

/// Um link reconhecido, antes de qualquer consulta de rede.
#[derive(Serialize, Clone, Debug)]
pub struct Candidate {
    pub url: String,
    /// `None` quando o link nao pertence a nenhuma fonte conhecida.
    pub source: Option<Source>,
    /// Identidade da novela, quando da para saber sem rede. O link de um video
    /// solto do TikTok nao diz qual serie e — so a consulta revela.
    pub drama_id: Option<String>,
    /// Ja esta na lista de acompanhadas.
    pub following: bool,
}

/// O que aconteceu com um item do lote.
#[derive(Serialize, Clone, Debug)]
pub struct Outcome {
    pub url: String,
    pub name: String,
    pub source: Option<Source>,
    pub episodes: usize,
    /// Episodios que ja estavam na pasta e nao foram baixados de novo.
    pub already: usize,
    pub followed: bool,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    /// Episodios que entraram na fila somando o lote inteiro.
    pub episodes: usize,
    /// Episodios do lote inteiro que ja estavam no disco.
    pub already: usize,
    pub followed: usize,
    pub cancelled: bool,
    /// Por que a rodada parou sozinha, quando parou. Irmao do `cancelled`, e o
    /// mesmo contrato do `watch::Report::stopped`.
    pub stopped: Option<String>,
    pub outcomes: Vec<Outcome>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Options {
    /// Passar a acompanhar cada novela do lote.
    pub follow: bool,
    /// Enfileirar todos os episodios disponiveis agora.
    pub download: bool,
}

/// Progresso de uma rodada, emitido antes de cada novela.
#[derive(Serialize, Clone, Debug)]
struct Progress {
    done: usize,
    total: usize,
    url: String,
    name: String,
    /// Segundos que ainda faltam, so com o ritmo de consulta. E o numero que
    /// impede a janela de parecer travada por quinze minutos.
    eta_secs: u64,
}

// ------------------------------------------------------------- extracao

/// Onde um link colado termina.
///
/// Pontuacao final e o caso comum: um link no fim de uma frase vem com ponto,
/// virgula ou parentese grudado, e leva-los junto quebraria a consulta.
fn trim_tail(mut s: &str) -> &str {
    while let Some(last) = s.chars().last() {
        if matches!(last, '.' | ',' | ';' | ':' | ')' | ']' | '}' | '!' | '?' | '\'' | '"') {
            s = &s[..s.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    s
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|')
}

/// Acha todos os links dentro de um texto qualquer.
///
/// Aceita tambem o link sem `https://` — e assim que ele sai de muito lugar
/// (aplicativo de mensagem, celular, planilha), e recusar isso devolveria ao
/// usuario o trabalho de arrumar cinquenta linhas na mao.
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let mut push = |raw: &str| {
        let url = trim_tail(raw).to_string();
        if url.is_empty() {
            return;
        }
        let key = url.trim_end_matches('/').to_lowercase();
        if seen.contains(&key) {
            return;
        }
        seen.push(key);
        out.push(url);
    };

    for token in text.split(is_boundary) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token.starts_with("http://") || token.starts_with("https://") {
            push(token);
            continue;
        }
        // Sem esquema: so vale se comecar num dominio conhecido, senao qualquer
        // palavra do texto viraria candidata.
        let bare = trim_tail(token);
        let low = bare.to_lowercase();
        if DOMAINS
            .iter()
            .any(|d| low.starts_with(d) || low.starts_with(&format!("www.{d}")))
        {
            push(&format!("https://{bare}"));
        }
    }
    out
}

/// De qual fonte e o link, e qual a identidade da novela — sem rede.
pub fn identify(url: &str) -> (Option<Source>, Option<String>) {
    if let Some((_, id)) = reelshort::parse_url(url) {
        return (Some(Source::Reelshort), Some(id));
    }
    if let Some((_, id)) = candyjar::parse_url(url) {
        return (Some(Source::Candyjar), Some(id));
    }
    if let Some((_, id)) = dramabox::parse_url(url) {
        return (Some(Source::Dramabox), Some(id));
    }
    if let Some(id) = tiktok::parse_drama_id(url) {
        return (Some(Source::Tiktok), Some(id));
    }
    if tiktok::parse_video_id(url).is_some() {
        // Link de video solto: e do TikTok, mas so a consulta diz qual serie.
        return (Some(Source::Tiktok), None);
    }
    (None, None)
}

/// Le o texto colado e devolve o que dara para fazer com ele — antes de gastar
/// uma unica requisicao.
///
/// Deduplica por identidade, e nao por URL: dois links da mesma novela (a
/// pagina da serie e a de um episodio) sao a mesma novela, e enfileirar as duas
/// gastaria duas consultas para o mesmo resultado.
pub fn preview(state: &AppState, text: &str) -> Vec<Candidate> {
    let seguindo: Vec<String> = watch::list(state).iter().map(|f| f.key()).collect();
    let mut out: Vec<Candidate> = Vec::new();
    let mut vistos: Vec<String> = Vec::new();

    for url in extract_urls(text) {
        let (source, drama_id) = identify(&url);
        // Identidade conhecida deduplica de verdade; sem ela (video solto do
        // TikTok) so resta a propria URL como chave.
        let key = match (source, &drama_id) {
            (Some(s), Some(id)) => s.cache_key(id),
            _ => url.to_lowercase(),
        };
        if vistos.contains(&key) {
            continue;
        }
        vistos.push(key.clone());
        let following = source.is_some() && drama_id.is_some() && seguindo.contains(&key);
        out.push(Candidate {
            url,
            source,
            drama_id,
            following,
        });
    }
    out
}

// -------------------------------------------------------------- a rodada

pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Processa a lista inteira, uma novela por vez, no ritmo da plataforma.
///
/// Devolve o relatorio no fim; o andamento sai por `bulk://progress` enquanto
/// roda, porque uma lista longa passa minutos sem nada mais acontecer na tela.
pub async fn run(app: &AppHandle, urls: Vec<String>, opts: Options) -> Report {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Report {
            outcomes: vec![Outcome {
                url: String::new(),
                name: String::new(),
                source: None,
                episodes: 0,
                already: 0,
                followed: false,
                error: Some("Ja existe um lote em andamento.".into()),
            }],
            ..Default::default()
        };
    }
    CANCEL.store(false, Ordering::SeqCst);

    let mut report = Report {
        total: urls.len(),
        ..Default::default()
    };

    for (i, url) in urls.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }

        // Piso de disco entre uma novela e outra — e nao um calculo unico antes
        // de comecar. O ReelShort e o CandyJar nao anunciam o tamanho dos
        // episodios, entao uma soma previa daria zero justo na fonte que o
        // usuario mais usa, e o lote passaria batido enchendo o volume. Parar
        // aqui deixa a fila coerente: o que ja entrou baixa, o resto nao entra.
        if opts.download {
            if let Some(parada) = piso_estourado(app) {
                report.stopped = Some(parada);
                break;
            }
        }

        // Quanto falta: so o ritmo de consulta, que domina o tempo da rodada.
        let restantes = urls.len().saturating_sub(i + 1) as u64;
        let _ = app.emit(
            "bulk://progress",
            Progress {
                done: i,
                total: urls.len(),
                url: url.clone(),
                name: String::new(),
                eta_secs: restantes * PACE.as_secs(),
            },
        );

        let outcome = process_one(app, url, &opts).await;
        if outcome.error.is_some() {
            report.failed += 1;
        } else {
            report.ok += 1;
            report.episodes += outcome.episodes;
            report.already += outcome.already;
            if outcome.followed {
                report.followed += 1;
            }
        }
        // O nome so existe depois da consulta: e este segundo aviso que troca
        // a URL crua pelo nome da novela na tela.
        let _ = app.emit(
            "bulk://progress",
            Progress {
                done: i + 1,
                total: urls.len(),
                url: url.clone(),
                name: outcome.name.clone(),
                eta_secs: restantes * PACE.as_secs(),
            },
        );
        report.outcomes.push(outcome);

        // Sem espera depois da ultima: ela so atrasaria o relatorio.
        if i + 1 < urls.len() && !CANCEL.load(Ordering::SeqCst) {
            // Em fatias de 1s para o cancelamento valer na hora, e nao daqui a
            // vinte segundos.
            for _ in 0..PACE.as_secs() {
                if CANCEL.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    RUNNING.store(false, Ordering::SeqCst);
    CANCEL.store(false, Ordering::SeqCst);
    queue::save(app);
    let _ = app.emit("bulk://done", report.clone());
    report
}

/// O volume de destino ja esta abaixo do piso configurado?
///
/// Reusa `auto_min_free_gb`: e o mesmo piso, e ter dois numeros para a mesma
/// ideia so criaria a chance de eles discordarem.
fn piso_estourado(app: &AppHandle) -> Option<String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let (dest, min_free_gb) = {
        let s = state.settings.lock().unwrap();
        (s.dest_dir.clone(), s.auto_min_free_gb)
    };
    let floor = (min_free_gb * GB as f64) as u64;
    let free = crate::disk::free_bytes(&dest)?;
    if free >= floor {
        return None;
    }
    Some(format!(
        "Parou: espaco livre abaixo do piso ({:.1} GB livres, piso de {:.1} GB).",
        free as f64 / GB as f64,
        min_free_gb
    ))
}

async fn process_one(app: &AppHandle, url: &str, opts: &Options) -> Outcome {
    let mut outcome = Outcome {
        url: url.to_string(),
        name: String::new(),
        source: identify(url).0,
        episodes: 0,
        already: 0,
        followed: false,
        error: None,
    };

    let series = match crate::load_series(app, url.to_string()).await {
        Ok(s) => s,
        Err(e) => {
            outcome.error = Some(e);
            return outcome;
        }
    };
    outcome.name = series.name.clone();
    outcome.source = Some(series.source);

    let state = app.state::<Arc<AppState>>().inner().clone();
    let dest = state.settings.lock().unwrap().dest_dir.clone();

    if opts.download {
        // Só o que falta. A pasta é consultada aqui pelo mesmo caminho que o
        // `enqueue` usa — não para decidir de novo (ele decide sozinho), mas
        // para poder DIZER quantos já estavam lá. Sem esse número, um lote de
        // cinquenta novelas já baixadas apareceria como "0 episódios" e
        // pareceria falha, quando é o resultado certo.
        let dir = media::series_dir(&dest, &series.name);
        let ja: std::collections::BTreeSet<u32> = crate::manifest::episode_files(&dir)
            .into_iter()
            .filter(|(_, nome)| crate::download::published_mp4(&dir.join(nome)))
            .map(|(n, _)| n)
            .collect();

        let episodios: Vec<u32> = series
            .episodes
            .iter()
            .filter(|e| media::episode_available(series.source, e) && !ja.contains(&e.number))
            .map(|e| e.number)
            .collect();
        outcome.already = series
            .episodes
            .iter()
            .filter(|e| ja.contains(&e.number))
            .count();

        if !episodios.is_empty() {
            outcome.episodes = queue::enqueue(&state, &series, &episodios, &dest, 1);
        }
    }
    // A ficha vai junto: e o mesmo contrato do `enqueue` da interface.
    queue::spawn_series_manifest(&state, &series, &dest);

    if opts.follow {
        outcome.followed = watch::add(
            &state,
            watch::Follow {
                source: series.source,
                drama_id: series.drama_id.clone(),
                name: series.name.clone(),
                url: url.to_string(),
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                last_check: 0,
                last_added: 0,
                paused: false,
                error: None,
            },
        );
        let _ = app.emit("watch://update", watch::list(&state));
    }

    // Sem media acessivel em nenhum episodio o item merece uma explicacao: sem
    // isso ele apareceria como sucesso silencioso e o usuario procuraria os
    // arquivos que nunca viriam.
    //
    // "Ja tenho tudo" tambem da zero episodios e NAO e erro — e o resultado
    // certo. Por isso a checagem exige que nada esteja no disco tambem.
    if opts.download && outcome.episodes == 0 && outcome.already == 0 {
        let trancados = series.episodes.iter().filter(|e| e.locked).count();
        if trancados == series.episodes.len() && trancados > 0 {
            outcome.error = Some(format!(
                "{} episodio(s), todos bloqueados para esta conta",
                trancados
            ));
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrai_links_de_texto_solto() {
        let texto = "olha essas: https://www.reelshort.com/pt/movie/a-69d8612ad6461266e80b7a17, \
                     e tambem candyjar.com/pt/series/x-725 (essa e boa)";
        let urls = extract_urls(texto);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("69d8612ad6461266e80b7a17"));
        assert_eq!(urls[1], "https://candyjar.com/pt/series/x-725");
    }

    #[test]
    fn ignora_repetido_e_texto_que_nao_e_link() {
        let texto = "bom dia https://candyjar.com/pt/series/x-725 https://candyjar.com/pt/series/x-725/";
        assert_eq!(extract_urls(texto).len(), 1);
    }

    #[test]
    fn identifica_a_fonte_sem_rede() {
        let (s, id) = identify("https://www.reelshort.com/pt/movie/a-69d8612ad6461266e80b7a17");
        assert_eq!(s, Some(Source::Reelshort));
        assert_eq!(id.as_deref(), Some("69d8612ad6461266e80b7a17"));

        let (s, _) = identify("https://exemplo.com/nada");
        assert_eq!(s, None);
    }

    #[test]
    fn tira_pontuacao_grudada() {
        assert_eq!(trim_tail("https://a.com/x)."), "https://a.com/x");
        assert_eq!(trim_tail("https://a.com/x"), "https://a.com/x");
    }
}
