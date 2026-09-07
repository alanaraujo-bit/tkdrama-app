//! Series acompanhadas e reabastecimento automatico da fila.
//!
//! O usuario aponta para uma serie UMA vez; daqui em diante o laco deste modulo
//! reconsulta cada serie seguida de tempos em tempos e enfileira o que tiver
//! aparecido de novo. E o que elimina o colar-link novela a novela.
//!
//! Tres decisoes carregam o resto do arquivo:
//!
//! - **Enfileira por ID de serie, nunca por URL de midia ja resolvida.** As URLs
//!   de midia sao assinadas e expiram; uma rodada que enfileirasse dezenas de
//!   episodios resolvidos veria a maioria morrer antes da vez. O caminho normal
//!   da fila ja reconsulta a API na hora de baixar.
//!
//! - **Nao existe contador de "ultimo episodio".** `queue::enqueue` ja ignora
//!   tarefa cujo id existe e ja marca episodio trancado como `Blocked`, entao
//!   reenviar a serie inteira a cada rodada da o mesmo resultado que um contador
//!   — sem um numero extra para dessincronizar do disco.
//!
//! - **Prioridade 0 (baixa).** O que entra sozinho nunca passa na frente do que
//!   o usuario acabou de pedir na mao.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use crate::disk;
use crate::media::Source;
use crate::queue::{self, AppState, Status};

/// Espera entre uma serie e a seguinte dentro da mesma rodada.
///
/// Nao e cortesia: rajada de consultas contra a API do TikTok e o que derruba a
/// sessao, e sessao morta para o app inteiro — nao so a rodada.
const PACE: Duration = Duration::from_secs(20);

/// De quanto em quanto tempo o laco acorda para ver se ja e hora de rodar.
///
/// Curto de proposito: quem decide a hora e `auto_interval_hours`; um tique
/// curto so faz mudanca de configuracao pegar rapido.
const TICK: Duration = Duration::from_secs(60);

const GB: u64 = 1024 * 1024 * 1024;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Uma serie que o usuario mandou acompanhar.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Follow {
    pub source: Source,
    pub drama_id: String,
    pub name: String,
    /// URL canonica da serie.
    ///
    /// Guardada em vez de remontada: o ReelShort precisa do idioma da pagina,
    /// que nao da para deduzir do `book_id` sozinho.
    pub url: String,
    pub added_at: u64,
    /// Ultima rodada que olhou esta serie (0 = nunca olhou).
    #[serde(default)]
    pub last_check: u64,
    /// Quantos episodios a ultima rodada acrescentou a fila.
    #[serde(default)]
    pub last_added: usize,
    #[serde(default)]
    pub paused: bool,
    /// Erro da ultima tentativa. Fica visivel na interface, mas nao interrompe
    /// as outras series da rodada.
    #[serde(default)]
    pub error: Option<String>,
}

impl Follow {
    pub fn key(&self) -> String {
        self.source.cache_key(&self.drama_id)
    }
}

/// Resultado de uma rodada, para a interface contar o que aconteceu.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    pub checked: usize,
    pub added: usize,
    pub bytes: u64,
    pub errors: Vec<String>,
    /// Motivo de a rodada ter parado antes de olhar tudo (teto, disco).
    pub stopped: Option<String>,
}

/// Bytes ainda por baixar no que esta na fila.
///
/// A diferenca antes/depois do `enqueue` diz quanto a rodada acrescentou de
/// verdade — `enqueue` devolve contagem de episodios, nao tamanho.
fn pending_bytes(state: &AppState) -> u64 {
    state
        .tasks
        .lock()
        .unwrap()
        .iter()
        .filter(|t| matches!(t.status, Status::Queued | Status::Running))
        .map(|t| t.size.saturating_sub(t.downloaded))
        .sum()
}

/// Descarta as tarefas `Blocked` desta serie para que a rodada as recrie.
///
/// Sem isto o bloqueio virava permanente: `enqueue` ignora id que ja existe,
/// entao um episodio marcado `Blocked` numa rodada em que o direito nao pode
/// ser consultado (janela do ReelShort fechada => so conteudo gratuito) nunca
/// mais seria reavaliado, mesmo com a conta VIP ativa. Recriar e barato e
/// idempotente: continuando bloqueado, volta a `Blocked` igual.
/// Devolve quantas descartou, para nao inflar a contagem de "novos" do
/// relatorio com episodios que apenas voltaram a ser recriados.
fn clear_blocked(state: &AppState, source: Source, drama_id: &str) -> usize {
    let mut tasks = state.tasks.lock().unwrap();
    let antes = tasks.len();
    tasks.retain(|t| {
        !(t.status == Status::Blocked && t.source == source && t.drama_id == drama_id)
    });
    antes - tasks.len()
}

pub fn list(state: &AppState) -> Vec<Follow> {
    state.follows.lock().unwrap().clone()
}

/// Acrescenta uma serie ao acompanhamento. Repetida e no-op: devolve `false`.
pub fn add(state: &AppState, follow: Follow) -> bool {
    let mut follows = state.follows.lock().unwrap();
    if follows.iter().any(|f| f.key() == follow.key()) {
        return false;
    }
    follows.push(follow);
    true
}

pub fn remove(state: &AppState, key: &str) {
    state.follows.lock().unwrap().retain(|f| f.key() != key);
}

pub fn set_paused(state: &AppState, key: &str, paused: bool) {
    if let Some(f) = state
        .follows
        .lock()
        .unwrap()
        .iter_mut()
        .find(|f| f.key() == key)
    {
        f.paused = paused;
    }
}

/// Uma rodada: reconsulta cada serie seguida e enfileira o que houver de novo.
///
/// `manual` vem do botao "Verificar agora" e ignora o interruptor do automatico
/// — quem clicou ja disse o que queria.
pub async fn run_once(app: &AppHandle, manual: bool) -> Report {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let mut report = Report::default();

    let (enabled, max_series, max_run_gb, min_free_gb, dest) = {
        let s = state.settings.lock().unwrap();
        (
            s.auto_enabled,
            s.auto_max_new_series,
            s.auto_max_gb_per_run,
            s.auto_min_free_gb,
            s.dest_dir.clone(),
        )
    };
    if !enabled && !manual {
        return report;
    }

    // Piso de disco antes de qualquer consulta: nao adianta descobrir episodio
    // novo para enfileirar sobre um volume que ja esta no limite.
    let floor = (min_free_gb * GB as f64) as u64;
    if let Some(free) = disk::free_bytes(&dest) {
        if free < floor {
            report.stopped = Some(format!(
                "Espaco livre abaixo do piso ({:.1} GB livres, piso de {:.1} GB).",
                free as f64 / GB as f64,
                min_free_gb
            ));
            return report;
        }
    }

    let alvos: Vec<Follow> = list(&state).into_iter().filter(|f| !f.paused).collect();
    let teto = (max_run_gb * GB as f64) as u64;
    let mut gastos: u64 = 0;

    for (i, follow) in alvos.iter().enumerate() {
        if max_series > 0 && i >= max_series {
            report.stopped = Some(format!("Teto de {max_series} series por rodada."));
            break;
        }
        if teto > 0 && gastos >= teto {
            report.stopped = Some(format!("Teto de {max_run_gb:.1} GB por rodada."));
            break;
        }
        // Espacamento entre series. Antes da consulta, nunca depois da ultima.
        if i > 0 {
            tokio::time::sleep(PACE).await;
        }

        let antes = pending_bytes(&state);
        let (added, erro) = match crate::load_series(app, follow.url.clone()).await {
            Ok(series) => {
                let recriados = clear_blocked(&state, series.source, &series.drama_id);
                let numeros: Vec<u32> = series.episodes.iter().map(|e| e.number).collect();
                let n = queue::enqueue(&state, &series, &numeros, &dest, queue::PRIO_LOW);
                (n.saturating_sub(recriados), None)
            }
            Err(e) => (0, Some(e)),
        };
        let delta = pending_bytes(&state).saturating_sub(antes);
        gastos += delta;
        report.checked += 1;
        report.added += added;
        report.bytes += delta;
        if let Some(e) = &erro {
            report.errors.push(format!("{}: {e}", follow.name));
        }

        // Anota o resultado nesta serie. Reencontra pela chave: a lista pode ter
        // mudado enquanto a consulta corria.
        let chave = follow.key();
        if let Some(f) = state
            .follows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|f| f.key() == chave)
        {
            f.last_check = now_secs();
            f.last_added = added;
            f.error = erro;
        }
    }

    if report.checked > 0 {
        state.mark_dirty();
        queue::save(app);
    }
    let _ = app.emit("watch://update", list(&state));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Task;

    fn seguir(source: Source, id: &str) -> Follow {
        Follow {
            source,
            drama_id: id.into(),
            name: format!("novela {id}"),
            url: format!("https://exemplo/{id}"),
            added_at: 0,
            last_check: 0,
            last_added: 0,
            paused: false,
            error: None,
        }
    }

    fn tarefa(source: Source, drama_id: &str, episode: u32, status: Status) -> Task {
        Task {
            id: format!("{}:{drama_id}-{episode}", source.tag()),
            drama_id: drama_id.into(),
            source,
            item_id: "i".into(),
            lang: String::new(),
            locked: status == Status::Blocked,
            series: "s".into(),
            episode,
            dest: "x.mp4".into(),
            gear_name: String::new(),
            codec: String::new(),
            width: 0,
            height: 0,
            duration: 0,
            size: 100,
            downloaded: 0,
            status,
            priority: queue::PRIO_LOW,
            error: None,
            seq: episode as u64,
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

    #[test]
    fn acompanhar_a_mesma_serie_duas_vezes_nao_duplica() {
        let state = AppState::new();
        assert!(add(&state, seguir(Source::Tiktok, "123")));
        assert!(!add(&state, seguir(Source::Tiktok, "123")));
        assert_eq!(list(&state).len(), 1);
    }

    /// Duas fontes podem usar o mesmo id sem se confundir: a chave leva o
    /// prefixo da fonte, como o id da tarefa.
    #[test]
    fn mesmo_id_em_fontes_diferentes_sao_series_distintas() {
        let state = AppState::new();
        assert!(add(&state, seguir(Source::Tiktok, "123")));
        assert!(add(&state, seguir(Source::Reelshort, "123")));
        assert_eq!(list(&state).len(), 2);

        remove(&state, &Source::Tiktok.cache_key("123"));
        let restante = list(&state);
        assert_eq!(restante.len(), 1);
        assert_eq!(restante[0].source, Source::Reelshort);
    }

    #[test]
    fn pausar_nao_remove_da_lista() {
        let state = AppState::new();
        add(&state, seguir(Source::Tiktok, "123"));
        set_paused(&state, &Source::Tiktok.cache_key("123"), true);
        assert!(list(&state)[0].paused);
    }

    /// O bloqueio precisa ser reavaliavel: uma rodada sem a janela do ReelShort
    /// aberta enxerga so o conteudo gratuito, e sem descartar os `Blocked` o
    /// `enqueue` (que ignora id ja existente) os congelaria para sempre.
    #[test]
    fn descarta_bloqueados_da_serie_e_preserva_o_resto() {
        let state = AppState::new();
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(tarefa(Source::Reelshort, "abc", 1, Status::Done));
            t.push(tarefa(Source::Reelshort, "abc", 2, Status::Blocked));
            t.push(tarefa(Source::Reelshort, "abc", 3, Status::Blocked));
            t.push(tarefa(Source::Reelshort, "outra", 1, Status::Blocked));
            t.push(tarefa(Source::Tiktok, "abc", 1, Status::Blocked));
        }
        assert_eq!(clear_blocked(&state, Source::Reelshort, "abc"), 2);

        let restam = state.tasks.lock().unwrap();
        assert_eq!(restam.len(), 3);
        // O concluido da propria serie fica: so o bloqueio e descartavel.
        assert!(restam
            .iter()
            .any(|t| t.source == Source::Reelshort && t.episode == 1 && t.status == Status::Done));
        // Bloqueado de OUTRA serie e da outra FONTE nao pode ser levado junto.
        assert!(restam.iter().any(|t| t.drama_id == "outra"));
        assert!(restam.iter().any(|t| t.source == Source::Tiktok));
    }

    /// So o que ainda vai baixar conta para o teto de GB da rodada: contar
    /// concluido faria a primeira rodada estourar o teto sozinha.
    #[test]
    fn bytes_pendentes_ignoram_concluido_e_bloqueado() {
        let state = AppState::new();
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(tarefa(Source::Tiktok, "a", 1, Status::Queued));
            t.push(tarefa(Source::Tiktok, "a", 2, Status::Done));
            t.push(tarefa(Source::Tiktok, "a", 3, Status::Blocked));
        }
        assert_eq!(pending_bytes(&state), 100);
    }
}

/// Laco de fundo. Acorda a cada `TICK` e roda quando o intervalo venceu.
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let state = app.state::<Arc<AppState>>().inner().clone();
            let (enabled, horas) = {
                let s = state.settings.lock().unwrap();
                (s.auto_enabled, s.auto_interval_hours.max(1))
            };
            if !enabled {
                continue;
            }
            let venceu = {
                let ultimo = *state.last_run.lock().unwrap();
                now_secs().saturating_sub(ultimo) >= horas * 3600
            };
            if !venceu {
                continue;
            }
            *state.last_run.lock().unwrap() = now_secs();
            let relatorio = run_once(&app, false).await;
            if relatorio.added > 0 {
                let _ = app.emit("watch://run", relatorio);
            }
        }
    });
}
