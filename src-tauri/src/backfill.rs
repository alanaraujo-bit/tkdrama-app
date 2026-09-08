//! Varredura de metadados: da ficha as novelas que ja estao no disco.
//!
//! O acrescimo de manifesto ao caminho de download so alcanca o que for baixado
//! de hoje em diante. A biblioteca atual — centenas de episodios ja gravados —
//! ficaria sem capa para sempre, e quem exibe a biblioteca nao pode busca-los:
//! ele nao fala com o TikTok, e nem deve.
//!
//! Entao esta varredura percorre as pastas que existem, consulta **so os
//! metadados da serie** (nenhum video e baixado) e grava `manifest.json`,
//! `poster.jpg` e `thumbs/`.
//!
//! O problema real aqui e de identidade: a pasta se chama pelo nome da novela e
//! nao carrega o id. Ele e procurado, nesta ordem, em tres lugares que ja
//! existem — o manifesto de uma passagem anterior, a fila e a lista de
//! acompanhadas. Pasta que nao se resolve e RELATADA, nunca adivinhada: um
//! palpite gravaria a ficha de uma novela na pasta de outra.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

use crate::manifest;
use crate::media::{self, Source};
use crate::queue::AppState;

/// Espera entre uma serie e a seguinte.
///
/// O mesmo numero e o mesmo motivo do laco de acompanhamento: rajada de
/// consultas contra a API do TikTok e o que derruba a sessao, e sessao morta
/// atinge o app inteiro, nao so a varredura. As imagens vem do CDN e nao
/// entram nessa conta.
const PACE: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    /// Pastas encontradas na biblioteca.
    pub folders: usize,
    /// Novelas que receberam manifesto nesta passagem.
    pub updated: usize,
    pub posters: usize,
    pub thumbs: usize,
    pub episodes: usize,
    pub converted: usize,
    /// Pastas cuja novela nao se pode identificar. Ficam sem ficha de
    /// proposito: preencher no chute e pior que deixar em branco.
    pub unresolved: Vec<String>,
    pub errors: Vec<String>,
}

/// Progresso, para a interface nao virar uma janela morta durante a varredura.
#[derive(Serialize, Clone, Debug)]
struct Progress {
    done: usize,
    total: usize,
    folder: String,
}

/// A novela de uma pasta, identificada.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub source: Source,
    pub drama_id: String,
    /// Idioma da pagina de origem; so o ReelShort precisa.
    pub lang: String,
    /// URL gravada no manifesto, quando ha uma.
    ///
    /// Vale mais que a remontada porque e a original: ela veio da pagina que o
    /// usuario abriu, com o idioma que aquela pagina usava.
    pub stored_url: String,
}

impl Identity {
    /// URL canonica da serie, que e por onde o carregamento entra.
    ///
    /// Passar pela URL em vez de chamar o adaptador direto e deliberado: e esse
    /// caminho que carrega a autenticacao das duas fontes — em especial a do
    /// ReelShort, que depende de perguntar o direito a janela logada.
    ///
    /// A gravada vence a remontada; a remontada continua existindo para os
    /// manifestos escritos antes de `sourceUrl` existir, que so tem id e fonte.
    pub fn url(&self) -> String {
        if !self.stored_url.trim().is_empty() {
            return self.stored_url.clone();
        }
        media::series_url(self.source, &self.drama_id, &self.lang)
    }
}

fn source_from_tag(tag: &str) -> Option<Source> {
    match tag.trim().to_ascii_lowercase().as_str() {
        "tiktok" => Some(Source::Tiktok),
        "reelshort" => Some(Source::Reelshort),
        "candyjar" => Some(Source::Candyjar),
        _ => None,
    }
}

/// Identifica a novela de uma pasta.
///
/// A ordem importa e vai do mais especifico ao mais frouxo:
///
/// 1. **O manifesto que ja esta la.** E o unico que fala daquela pasta e de
///    mais nenhuma.
/// 2. **A fila**, por uma tarefa cujo arquivo de destino mora nesta pasta. E
///    tambem o unico lugar que sabe o idioma de uma serie do ReelShort.
/// 3. **A lista de acompanhadas**, casando o nome. Vem por ultimo porque duas
///    novelas de nome parecido cairiam na mesma pasta higienizada.
pub fn identify(
    dir: &Path,
    tasks: &[crate::queue::Task],
    follows: &[crate::watch::Follow],
) -> Option<Identity> {
    let m = manifest::read(dir);
    if !m.drama_id.is_empty() {
        if let Some(source) = source_from_tag(&m.source) {
            // O manifesto nao guarda idioma; a fila abaixo pode completa-lo.
            let lang = tasks
                .iter()
                .find(|t| t.drama_id == m.drama_id && !t.lang.is_empty())
                .map(|t| t.lang.clone())
                .unwrap_or_default();
            return Some(Identity {
                source,
                drama_id: m.drama_id,
                lang,
                stored_url: m.source_url,
            });
        }
    }

    if let Some(t) = tasks
        .iter()
        .find(|t| PathBuf::from(&t.dest).parent() == Some(dir))
    {
        return Some(Identity {
            source: t.source,
            drama_id: t.drama_id.clone(),
            lang: t.lang.clone(),
            stored_url: String::new(),
        });
    }

    let nome = dir.file_name()?.to_string_lossy().to_string();
    let f = follows.iter().find(|f| media::safe_name(&f.name) == nome)?;
    Some(Identity {
        source: f.source,
        drama_id: f.drama_id.clone(),
        // A URL guardada na serie acompanhada e a fonte do idioma; ela vence a
        // reconstrucao, e por isso o chamador prefere `f.url`. So ReelShort e
        // CandyJar usam idioma; cada um tem seu proprio formato de URL.
        lang: match f.source {
            Source::Reelshort => crate::reelshort::parse_url(&f.url).map(|(lang, _)| lang),
            Source::Candyjar => crate::candyjar::parse_url(&f.url).map(|(lang, _)| lang),
            Source::Tiktok => None,
        }
        .unwrap_or_default(),
        stored_url: String::new(),
    })
}

/// Percorre a biblioteca e escreve os metadados do que ja esta baixado.
///
/// Nenhum video e baixado: so a consulta de metadados da serie, mais a capa e
/// as miniaturas, que sao imagens.
pub async fn run(app: &AppHandle) -> Result<Report, String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let dest = state.settings.lock().unwrap().dest_dir.clone();
    let raiz = PathBuf::from(&dest);

    let mut pastas: Vec<PathBuf> = std::fs::read_dir(&raiz)
        .map_err(|e| format!("Nao consegui ler a pasta da biblioteca ({dest}): {e}"))?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(true)
        })
        .collect();
    pastas.sort();

    let mut report = Report {
        folders: pastas.len(),
        ..Default::default()
    };

    let tasks = state.snapshot();
    let follows = crate::watch::list(&state);
    // Resolve tudo antes de sair para a rede: assim a contagem de pastas sem
    // identidade aparece de imediato, sem esperar a fila de consultas.
    // Primeiro repara tudo que depende apenas do disco. Assim `.ts` antigos,
    // thumbnails ausentes e manifestos incompletos não dependem de login.
    for dir in &pastas {
        match manifest::backfill_local(dir).await {
            Ok(local) => {
                report.converted += local.converted;
                report.thumbs += local.thumbs;
                report.episodes += local.episodes;
                report.errors.extend(local.errors.into_iter().map(|e| format!("{}: {e}", dir.display())));
            }
            Err(e) => report.errors.push(format!("{}: {e}", dir.display())),
        }
    }

    let alvos: Vec<(PathBuf, String)> = pastas
        .into_iter()
        .filter_map(|dir| {
            let nome = dir.file_name()?.to_string_lossy().to_string();
            match identify(&dir, &tasks, &follows) {
                Some(id) => {
                    // A URL do manifesto descreve esta pasta especificamente e
                    // por isso vence a acompanhada. Sem ela, a acompanhada ainda
                    // vence a remontada porque preserva o idioma do navegador.
                    let url = if !id.stored_url.trim().is_empty() {
                        id.url()
                    } else {
                        follows
                            .iter()
                            .find(|f| f.source == id.source && f.drama_id == id.drama_id)
                            .map(|f| f.url.clone())
                            .filter(|u| !u.trim().is_empty())
                            .unwrap_or_else(|| id.url())
                    };
                    Some((dir, url))
                }
                None => {
                    report.unresolved.push(nome);
                    None
                }
            }
        })
        .collect();

    let total = alvos.len();
    for (i, (dir, url)) in alvos.into_iter().enumerate() {
        let nome = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let _ = app.emit(
            "meta://progress",
            Progress {
                done: i,
                total,
                folder: nome.clone(),
            },
        );
        // Espacamento antes da consulta, nunca depois da ultima.
        if i > 0 {
            tokio::time::sleep(PACE).await;
        }

        match crate::load_series(app, url).await {
            Ok(series) => match crate::queue::sync_folder(&state, &series, &dir).await {
                Ok(c) => {
                    report.updated += 1;
                    report.posters += usize::from(c.poster);
                    report.thumbs += c.thumbs;
                    report.episodes += c.episodes;
                }
                Err(e) => report.errors.push(format!("{nome}: {e}")),
            },
            // Falta de login nao e falha desta novela: e de todas. Continuar
            // gastaria 20 segundos por pasta para colecionar a mesma frase, e
            // o usuario ficaria minutos olhando um botao parado. Para na hora.
            Err(e) if crate::tiktok::is_auth_error(&e) => {
                report.errors.push(e);
                break;
            }
            // Uma novela que saiu do ar nao pode derrubar a varredura das
            // outras: o erro entra no relatorio e a fila segue.
            Err(e) => report.errors.push(format!("{nome}: {e}")),
        }
    }

    let _ = app.emit(
        "meta://progress",
        Progress {
            done: total,
            total,
            folder: String::new(),
        },
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Status, Task};
    use crate::watch::Follow;

    fn dir_de_teste(nome: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tkdrama_backfill_{nome}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tarefa(source: Source, drama_id: &str, lang: &str, dest: &Path) -> Task {
        Task {
            id: format!("{}:{drama_id}-1", source.tag()),
            drama_id: drama_id.into(),
            source,
            item_id: "i".into(),
            lang: lang.into(),
            locked: false,
            series: "s".into(),
            episode: 1,
            dest: dest.to_string_lossy().to_string(),
            gear_name: String::new(),
            codec: String::new(),
            width: 0,
            height: 0,
            duration: 0,
            size: 0,
            downloaded: 0,
            status: Status::Done,
            priority: 1,
            error: None,
            seq: 1,
            refreshes: 0,
            urls: vec![],
            speed: 0.0,
            attempts: 0,
            retry_at: 0,
            elapsed_secs: 0,
            created_at: 0,
            finished_at: None,
            active_since: None,
        }
    }

    fn seguida(source: Source, drama_id: &str, name: &str, url: &str) -> Follow {
        Follow {
            source,
            drama_id: drama_id.into(),
            name: name.into(),
            url: url.into(),
            added_at: 0,
            last_check: 0,
            last_added: 0,
            paused: false,
            error: None,
        }
    }

    /// Os manifestos ja existentes nao tem `sourceUrl`. Eles precisam continuar
    /// resolvendo pela combinacao de id e fonte para a migracao nao abandonar a
    /// biblioteca gravada antes desta chave existir.
    #[test]
    fn manifesto_antigo_sem_source_url_ainda_resolve() {
        let dir = dir_de_teste("manifesto");
        let m = manifest::Manifest {
            drama_id: "7677784436146164743".into(),
            source: "tiktok".into(),
            ..Default::default()
        };
        manifest::write(&dir, &m).unwrap();

        let id = identify(&dir, &[], &[]).unwrap();
        assert_eq!(id.source, Source::Tiktok);
        assert_eq!(id.drama_id, "7677784436146164743");
        assert_eq!(
            id.url(),
            "https://www.tiktok.com/shortdrama/episode/7677784436146164743/1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No ReelShort, o idioma so sobrevive na URL original. Remontar pelo id
    /// assumiria `pt` e carregaria outra versao da novela sem acusar erro.
    #[test]
    fn manifesto_preserva_o_idioma_da_url_do_reelshort() {
        let dir = dir_de_teste("manifesto_reelshort_en");
        let m = manifest::Manifest {
            drama_id: "69d8612ad6461266e80b7a17".into(),
            source: "reelshort".into(),
            source_url: "https://www.reelshort.com/en/movie/69d8612ad6461266e80b7a17".into(),
            ..Default::default()
        };
        manifest::write(&dir, &m).unwrap();

        let id = identify(&dir, &[], &[]).unwrap();
        let (lang, book) = crate::reelshort::parse_url(&id.url()).unwrap();
        assert_eq!(lang, "en");
        assert_eq!(book, "69d8612ad6461266e80b7a17");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sem manifesto, a fila responde — e e o unico lugar que sabe o idioma de
    /// uma serie do ReelShort, sem o qual a URL da serie nao se remonta.
    #[test]
    fn a_fila_identifica_e_traz_o_idioma() {
        let dir = dir_de_teste("fila");
        let t = tarefa(
            Source::Reelshort,
            "69d8612ad6461266e80b7a17",
            "pt",
            &dir.join("Novela - E01.ts"),
        );
        let id = identify(&dir, std::slice::from_ref(&t), &[]).unwrap();
        assert_eq!(id.source, Source::Reelshort);
        assert_eq!(id.lang, "pt");
        assert_eq!(
            id.url(),
            "https://www.reelshort.com/pt/movie/69d8612ad6461266e80b7a17"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ultimo recurso: o nome da pasta, que e o nome da novela higienizado.
    #[test]
    fn a_lista_de_acompanhadas_identifica_pelo_nome() {
        // A pasta se chama pelo nome da novela higienizado: e so por ele que a
        // lista de acompanhadas consegue reconhece-la.
        let raiz = dir_de_teste("por_nome");
        let dir = raiz.join("A cura mortal do bilionario");
        std::fs::create_dir_all(&dir).unwrap();
        let f = seguida(
            Source::Tiktok,
            "7677784436146164743",
            "A cura mortal do bilionario",
            "https://www.tiktok.com/shortdrama/episode/7677784436146164743/1",
        );
        let id = identify(&dir, &[], std::slice::from_ref(&f)).unwrap();
        assert_eq!(id.drama_id, "7677784436146164743");
        let _ = std::fs::remove_dir_all(&raiz);
    }

    /// Pasta que nao se identifica fica sem ficha, e isso e o certo: escrever a
    /// novela errada numa pasta e pior que deixa-la em branco.
    #[test]
    fn pasta_sem_identidade_nao_e_adivinhada() {
        let dir = dir_de_teste("desconhecida");
        assert!(identify(&dir, &[], &[]).is_none());

        // Nem uma tarefa de OUTRA pasta serve de palpite.
        let outra = dir.parent().unwrap().join("tkdrama_backfill_outra");
        let t = tarefa(Source::Tiktok, "123456", "", &outra.join("x - E01.mp4"));
        assert!(identify(&dir, std::slice::from_ref(&t), &[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Um manifesto sem `source` legivel nao identifica nada: sem saber a
    /// fonte, o id sozinho pode ser de qualquer uma das duas.
    #[test]
    fn manifesto_sem_fonte_nao_basta() {
        let dir = dir_de_teste("sem_fonte");
        let m = manifest::Manifest {
            drama_id: "123456".into(),
            ..Default::default()
        };
        manifest::write(&dir, &m).unwrap();
        assert!(identify(&dir, &[], &[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_url_remontada_e_reconhecida_de_volta() {
        let tk = Identity {
            source: Source::Tiktok,
            drama_id: "7677784436146164743".into(),
            lang: String::new(),
            stored_url: String::new(),
        };
        assert_eq!(
            crate::tiktok::parse_drama_id(&tk.url()).as_deref(),
            Some("7677784436146164743")
        );

        // Sem idioma anotado, "pt" — e a URL continua valida.
        let rs = Identity {
            source: Source::Reelshort,
            drama_id: "69d8612ad6461266e80b7a17".into(),
            lang: String::new(),
            stored_url: String::new(),
        };
        let (lang, book) = crate::reelshort::parse_url(&rs.url()).unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(book, "69d8612ad6461266e80b7a17");

        // CandyJar: sem idioma anotado, "en" — o default do proprio site.
        let cj = Identity {
            source: Source::Candyjar,
            drama_id: "725".into(),
            lang: String::new(),
            stored_url: String::new(),
        };
        let (lang, id) = crate::candyjar::parse_url(&cj.url()).unwrap();
        assert_eq!(lang, "en");
        assert_eq!(id, "725");
    }

    #[test]
    fn manifesto_preserva_o_idioma_da_url_do_candyjar() {
        let dir = dir_de_teste("manifesto_candyjar_pt");
        let m = manifest::Manifest {
            drama_id: "725".into(),
            source: "candyjar".into(),
            source_url: "https://candyjar.com/pt/series/725".into(),
            ..Default::default()
        };
        manifest::write(&dir, &m).unwrap();

        let id = identify(&dir, &[], &[]).unwrap();
        let (lang, book) = crate::candyjar::parse_url(&id.url()).unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(book, "725");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
