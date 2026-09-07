//! Motor da fila: prioridade, concorrencia, pausa/retomada e persistencia.
//!
//! Regras centrais:
//! - O agendador roda num laco proprio e escolhe sempre a tarefa de maior
//!   prioridade (empate = ordem de entrada).
//! - As URLs de midia sao assinadas e expiram, entao NAO sao persistidas: ao
//!   iniciar uma tarefa sem URL valida, a serie e consultada de novo.
//! - Nenhum lock e mantido atravessando `.await` (copia-se o necessario antes).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

use crate::download::{self, Control, Outcome, CANCEL, PAUSE, RUN};
use crate::media::{self, Series, Source};
use crate::reelshort;
use crate::tiktok;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    /// Episodio sem midia acessivel (exige desbloqueio no TikTok).
    Blocked,
}

// A interface envia a prioridade como numero; estas constantes nomeiam os
// valores para o codigo e os testes.
#[allow(dead_code)]
pub const PRIO_LOW: u8 = 0;
#[allow(dead_code)]
pub const PRIO_NORMAL: u8 = 1;
pub const PRIO_HIGH: u8 = 2;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id: String,
    pub drama_id: String,
    /// De onde veio: decide quem resolve a midia e como se baixa.
    #[serde(default)]
    pub source: Source,
    /// Id do episodio na fonte (chapter_id no ReelShort). Sem ele nao da para
    /// resolver a midia depois, e no ReelShort ela so e resolvida na hora.
    #[serde(default)]
    pub item_id: String,
    /// Idioma da pagina de origem; so o ReelShort usa.
    #[serde(default)]
    pub lang: String,
    /// Episodio pago/trancado na plataforma. Fica no proprio Task, e nao
    /// inferido do status: o agendador troca o status para `Running` antes de
    /// chamar quem baixa, entao olhar o status ali nunca veria o bloqueio.
    #[serde(default)]
    pub locked: bool,
    pub series: String,
    pub episode: u32,
    pub dest: String,
    pub gear_name: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub duration: u32,
    pub size: u64,
    pub downloaded: u64,
    pub status: Status,
    pub priority: u8,
    pub error: Option<String>,
    pub seq: u64,
    /// Renovacoes de link ja tentadas nesta tarefa (limite anti-loop).
    #[serde(default)]
    pub refreshes: u8,
    /// URLs assinadas: transitorias de proposito, renovadas sob demanda.
    #[serde(skip)]
    pub urls: Vec<String>,
    /// Velocidade instantanea em B/s. Precisa chegar na interface: e dela que
    /// saem a coluna de velocidade, o ETA da linha e o total do rodape.
    #[serde(default)]
    pub speed: f64,
    /// Tentativas automaticas ja gastas nesta tarefa (ver `MAX_ATTEMPTS`).
    #[serde(default)]
    pub attempts: u8,
    /// Unix seconds antes dos quais a tarefa nao deve ser reiniciada (backoff).
    #[serde(default)]
    pub retry_at: u64,
    /// Segundos efetivamente gastos baixando (pausas nao entram na conta).
    #[serde(default)]
    pub elapsed_secs: u64,
    /// Momento em que entrou na fila; alimenta o tempo total da sessão.
    #[serde(default)]
    pub created_at: u64,
    /// Momento em que parou de vez (concluida ou falhada). Congela o relogio
    /// do rodape: sem isso o "decorrido" continua correndo com a fila parada.
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// Inicio do trecho atual, em Unix seconds. E transitorio para a UI poder
    /// exibir o relogio correndo sem gravar o estado quatro vezes por segundo.
    #[serde(skip)]
    pub active_since: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub dest_dir: String,
    pub concurrency: usize,
    pub codec: String,
    pub gear: Option<String>,
    /// Uma conexao por vez, sempre na ordem dos episodios.
    #[serde(default)]
    pub sequential: bool,

    // -------------------------------------------------- reabastecimento automatico
    /// Liga o laco que reconsulta as series seguidas sozinho (ver `watch.rs`).
    #[serde(default)]
    pub auto_enabled: bool,
    /// De quantas em quantas horas uma rodada acontece.
    #[serde(default = "default_interval")]
    pub auto_interval_hours: u64,
    /// Teto de series olhadas por rodada. 0 = sem teto.
    #[serde(default = "default_max_series")]
    pub auto_max_new_series: usize,
    /// Teto de GB enfileirados por rodada. 0 = sem teto.
    #[serde(default = "default_max_gb")]
    pub auto_max_gb_per_run: f64,
    /// Abaixo deste espaco livre a rodada nem comeca.
    #[serde(default = "default_min_free")]
    pub auto_min_free_gb: f64,
}

fn default_interval() -> u64 {
    6
}
fn default_max_series() -> usize {
    10
}
fn default_max_gb() -> f64 {
    5.0
}
fn default_min_free() -> f64 {
    10.0
}

impl Default for Settings {
    fn default() -> Self {
        let dir = dirs_videos().unwrap_or_else(|| PathBuf::from("."));
        Self {
            dest_dir: dir.to_string_lossy().to_string(),
            concurrency: 3,
            codec: "h264".into(),
            gear: None,
            sequential: false,
            auto_enabled: false,
            auto_interval_hours: default_interval(),
            auto_max_new_series: default_max_series(),
            auto_max_gb_per_run: default_max_gb(),
            auto_min_free_gb: default_min_free(),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn dirs_videos() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Videos").join("Novelas TikTok"))
}

pub struct AppState {
    pub tasks: Mutex<Vec<Task>>,
    pub controls: Mutex<HashMap<String, Control>>,
    pub settings: Mutex<Settings>,
    /// Cookies por fonte. Eram um campo unico quando so existia o TikTok;
    /// misturar os dois mandaria cookie de um site para o outro.
    pub cookies: Mutex<HashMap<Source, String>>,
    /// Sessao do ReelShort: cookie + credencial de cabecalho capturada da
    /// janela logada. So o cookie nao autentica a API deles.
    pub rs_session: Mutex<reelshort::Session>,
    pub series_cache: Mutex<HashMap<String, Series>>,
    /// Series que o usuario mandou acompanhar (ver `watch.rs`).
    pub follows: Mutex<Vec<crate::watch::Follow>>,
    /// Quando a ultima rodada automatica correu. Persistido para que fechar e
    /// reabrir o app nao dispare uma rodada imediata a cada vez.
    pub last_run: Mutex<u64>,
    pub client: reqwest::Client,
    pub seq: AtomicU64,
    pub dirty: AtomicU8,
}

impl AppState {
    pub fn new() -> Self {
        // NAO usar `.timeout()`: em reqwest ele e um prazo TOTAL, leitura do
        // corpo inclusa, e mataria todo episodio que demorasse mais que ele.
        // O que precisa de limite e a conexao parada, nao o download longo.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(45))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("cliente http");
        Self {
            tasks: Mutex::new(Vec::new()),
            controls: Mutex::new(HashMap::new()),
            settings: Mutex::new(Settings::default()),
            cookies: Mutex::new(HashMap::new()),
            rs_session: Mutex::new(reelshort::Session::default()),
            series_cache: Mutex::new(HashMap::new()),
            follows: Mutex::new(Vec::new()),
            last_run: Mutex::new(0),
            client,
            seq: AtomicU64::new(0),
            dirty: AtomicU8::new(1),
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Vec<Task> {
        self.tasks.lock().unwrap().clone()
    }

    /// Sessao do ReelShort: cookie atual + credencial ja capturada.
    pub fn rs_session(&self) -> reelshort::Session {
        let mut sess = self.rs_session.lock().unwrap().clone();
        sess.cookie = self.cookie_for(Source::Reelshort);
        sess
    }

    /// Header Cookie da fonte pedida (vazio quando nao ha sessao).
    pub fn cookie_for(&self, source: Source) -> String {
        self.cookies
            .lock()
            .unwrap()
            .get(&source)
            .cloned()
            .unwrap_or_default()
    }

    fn control_for(&self, id: &str) -> Control {
        let mut map = self.controls.lock().unwrap();
        map.entry(id.to_string())
            .or_insert_with(|| Arc::new(AtomicU8::new(RUN)))
            .clone()
    }
}

// ------------------------------------------------------------------ persistencia

fn state_file(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("state.json"))
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    settings: Settings,
    tasks: Vec<Task>,
    /// `default` para que um estado gravado antes do acompanhamento carregue
    /// sem erro em vez de ser descartado inteiro por um campo ausente.
    #[serde(default)]
    follows: Vec<crate::watch::Follow>,
    #[serde(default)]
    last_run: u64,
}

pub fn save(app: &AppHandle) {
    let Some(path) = state_file(app) else { return };
    let state = app.state::<Arc<AppState>>();
    let mut tasks = state.snapshot();
    // "Running" nao sobrevive ao fechamento: volta para a fila.
    for t in &mut tasks {
        if t.status == Status::Running {
            t.status = Status::Queued;
        }
        t.speed = 0.0;
        t.active_since = None;
    }
    let data = Persisted {
        settings: state.settings.lock().unwrap().clone(),
        tasks,
        follows: state.follows.lock().unwrap().clone(),
        last_run: *state.last_run.lock().unwrap(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&data) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn load(app: &AppHandle) {
    let Some(path) = state_file(app) else { return };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(data) = serde_json::from_str::<Persisted>(&text) else {
        return;
    };
    let state = app.state::<Arc<AppState>>();
    *state.settings.lock().unwrap() = data.settings;
    *state.follows.lock().unwrap() = data.follows;
    *state.last_run.lock().unwrap() = data.last_run;
    let mut tasks = data.tasks;
    let max_seq = tasks.iter().map(|t| t.seq).max().unwrap_or(0);
    // Reconciliacao com o disco: o estado salvo e uma anotacao, o disco e a
    // verdade. Um arquivo apagado por fora nao pode continuar "concluido", e
    // um .part precisa reaparecer com os bytes que realmente tem.
    for t in &mut tasks {
        // Estado salvo antes das duas fontes usava id sem prefixo. Sem migrar,
        // o `enqueue` nao reconheceria a tarefa como ja existente e criaria uma
        // segunda apontando para o MESMO arquivo — duas gravando no mesmo .part.
        if !t.id.contains(':') {
            t.id = task_id(t.source, &t.drama_id, t.episode);
        }
        if t.status == Status::Running {
            t.status = Status::Queued;
        }
        t.active_since = None;
        t.speed = 0.0;
        t.retry_at = 0;
        let dest = PathBuf::from(&t.dest);
        if t.status == Status::Done {
            match std::fs::metadata(&dest) {
                Ok(m) if t.size == 0 || m.len() == t.size => {
                    t.downloaded = m.len();
                    if t.size == 0 {
                        t.size = m.len();
                    }
                }
                _ => {
                    // Sumiu ou esta truncado: volta para a fila.
                    t.status = Status::Queued;
                    t.attempts = 0;
                    t.finished_at = None;
                    t.error = None;
                    t.downloaded = 0;
                }
            }
        }
        if t.status != Status::Done {
            let part = std::fs::metadata(download::part_path(&dest))
                .map(|m| m.len())
                .unwrap_or(0);
            t.downloaded = if t.size > 0 { part.min(t.size) } else { part };
        }
    }
    *state.tasks.lock().unwrap() = tasks;
    state.seq.store(max_seq + 1, Ordering::Relaxed);
    state.mark_dirty();
}

// -------------------------------------------------------------------- agendador

/// Quantas vezes uma tarefa se recupera sozinha antes de desistir de vez.
pub const MAX_ATTEMPTS: u8 = 6;

/// Espera antes da proxima tentativa: cresce, mas nunca passa de 2 minutos.
fn backoff_secs(attempts: u8) -> u64 {
    match attempts {
        0 | 1 => 3,
        2 => 10,
        3 => 30,
        4 => 60,
        _ => 120,
    }
}

/// Escolhe a proxima tarefa: maior prioridade, empate pela ordem de entrada.
/// Tarefas em espera de backoff ficam de fora ate a hora chegar.
fn pick_next(state: &AppState) -> Option<String> {
    let now = now_secs();
    let tasks = state.tasks.lock().unwrap();
    tasks
        .iter()
        .filter(|t| t.status == Status::Queued && t.retry_at <= now)
        .min_by_key(|t| (std::cmp::Reverse(t.priority), t.seq))
        .map(|t| t.id.clone())
}

/// Ha alguma tarefa aguardando o relogio do backoff? Enquanto houver, a
/// interface precisa continuar recebendo atualizacoes para mostrar a contagem.
fn has_pending_retry(state: &AppState) -> bool {
    let now = now_secs();
    state
        .tasks
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.status == Status::Queued && t.retry_at > now)
}

fn running_count(state: &AppState) -> usize {
    state
        .tasks
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.status == Status::Running)
        .count()
}

pub fn start_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<Arc<AppState>>().inner().clone();
        let mut ticks: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            ticks += 1;

            // TikTok passa URLs assinadas e tende a derrubar rajadas grandes.
            // Mesmo no modo paralelo, tres fluxos e o teto deliberado.
            let settings = state.settings.lock().unwrap().clone();
            let limit = if settings.sequential { 1 } else { settings.concurrency.clamp(1, 3) };
            while running_count(&state) < limit {
                let Some(id) = pick_next(&state) else { break };
                {
                    let mut tasks = state.tasks.lock().unwrap();
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                        t.status = Status::Running;
                        t.error = None;
                        t.retry_at = 0;
                        t.finished_at = None;
                        t.active_since = Some(now_secs());
                    }
                }
                state.control_for(&id).store(RUN, Ordering::Relaxed);
                spawn_download(app.clone(), state.clone(), id);
                state.mark_dirty();
            }

            // Emite no maximo ~4x/s, e so quando algo mudou. Tarefas em backoff
            // contam como "vivas": a contagem regressiva precisa andar na tela.
            let alive = running_count(&state) > 0 || has_pending_retry(&state);
            if state.dirty.swap(0, Ordering::Relaxed) == 1 || alive {
                let _ = app.emit("queue://update", state.snapshot());
            }
            // Salva de tempos em tempos para sobreviver a um fechamento abrupto.
            if ticks % 40 == 0 {
                save(&app);
            }
        }
    });
}

/// Garante URLs validas para a tarefa, consultando a API quando preciso.
async fn refresh_urls(state: &Arc<AppState>, id: &str) -> anyhow::Result<()> {
    let (source, drama_id, episode, item_id, lang) = {
        let tasks = state.tasks.lock().unwrap();
        let t = tasks
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("tarefa sumiu"))?;
        (
            t.source,
            t.drama_id.clone(),
            t.episode,
            t.item_id.clone(),
            t.lang.clone(),
        )
    };
    if source == Source::Reelshort {
        return refresh_reelshort(state, id, &drama_id, episode, &item_id, &lang).await;
    }
    let cookie = state.cookie_for(Source::Tiktok);
    let series = tiktok::fetch_series(&state.client, &drama_id, &cookie).await?;

    let (codec, gear_override) = {
        let s = state.settings.lock().unwrap();
        (s.codec.clone(), s.gear.clone())
    };
    let ep = series
        .episodes
        .iter()
        .find(|e| e.number == episode)
        .ok_or_else(|| anyhow::anyhow!("episodio {episode} nao existe mais"))?;
    let gear = media::pick_gear(ep, &codec, gear_override.as_deref())
        .ok_or_else(|| anyhow::anyhow!("episodio sem midia acessivel"))?;

    {
        let mut tasks = state.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.urls = gear.urls.clone();
            t.size = gear.size;
            t.gear_name = gear.gear_name.clone();
            t.codec = gear.codec.clone();
            t.width = gear.width;
            t.height = gear.height;
        }
    }
    state
        .series_cache
        .lock()
        .unwrap()
        .insert(Source::Tiktok.cache_key(&drama_id), series.clone());
    Ok(())
}

/// Resolve a midia de um episodio do ReelShort na hora de baixar.
///
/// A pagina da serie nao traz URL de video: cada episodio exige a sua propria
/// requisicao. Fazer isso na listagem seria uma requisicao por episodio so para
/// mostrar a lista — com 93 episodios, um minuto de espera e um martelo no site.
async fn refresh_reelshort(
    state: &Arc<AppState>,
    id: &str,
    book_id: &str,
    episode: u32,
    chapter_id: &str,
    lang: &str,
) -> anyhow::Result<()> {
    // Barreira do paywall: se a tarefa esta trancada, nem a requisicao sai.
    // Le a marca gravada na tarefa, nao o status — quando este codigo roda o
    // agendador ja pos a tarefa em `Running`, entao o status nunca diria isso.
    let locked = {
        let tasks = state.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.locked)
            .unwrap_or(true)
    };
    let sess = state.rs_session();
    let (segments, duration) = reelshort::resolve_media(
        &state.client,
        lang,
        book_id,
        episode,
        chapter_id,
        locked,
        &sess,
    )
    .await?;

    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        t.urls = segments;
        t.gear_name = reelshort::GEAR_NAME.to_string();
        t.codec = "h264".into();
        if duration > 0 {
            t.duration = duration;
        }
    }
    Ok(())
}

fn spawn_download(app: AppHandle, state: Arc<AppState>, id: String) {
    tauri::async_runtime::spawn(async move {
        // URLs assinadas expiram: renova antes de comecar se necessario.
        let needs_urls = {
            let tasks = state.tasks.lock().unwrap();
            tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.urls.is_empty())
                .unwrap_or(true)
        };
        if needs_urls {
            if let Err(e) = refresh_urls(&state, &id).await {
                let msg = e.to_string();
                // "Nao existe mais" e definitivo; qualquer outra coisa (rede,
                // sessao, API fora do ar) merece nova tentativa automatica.
                if msg.contains("nao existe mais") || msg.contains("sem midia acessivel") {
                    finish(&state, &id, Status::Blocked, Some(friendly(&msg)));
                } else if is_auth_error(&msg) {
                    // Repetir nao resolve login: falha logo, com a instrucao certa.
                    finish(&state, &id, Status::Failed, Some(friendly(&msg)));
                } else if !schedule_retry(&state, &id, &friendly(&msg), true) {
                    finish(&state, &id, Status::Failed, Some(friendly(&msg)));
                }
                return;
            }
        }

        let (gear, dest, source) = {
            let tasks = state.tasks.lock().unwrap();
            let Some(t) = tasks.iter().find(|t| t.id == id) else {
                // A tarefa sumiu no meio do caminho (cancelada). Solta a vaga
                // do agendador: sair sem limpar deixaria um "Running" fantasma
                // ocupando um dos tres canais para sempre.
                drop(tasks);
                state.controls.lock().unwrap().remove(&id);
                state.mark_dirty();
                return;
            };
            (
                media::Gear {
                    gear_name: t.gear_name.clone(),
                    codec: t.codec.clone(),
                    width: t.width,
                    height: t.height,
                    bitrate: 0,
                    size: t.size,
                    urls: t.urls.clone(),
                },
                PathBuf::from(&t.dest),
                t.source,
            )
        };

        if gear.urls.is_empty() {
            finish(&state, &id, Status::Blocked, Some("sem midia acessivel".into()));
            return;
        }

        let control = state.control_for(&id);
        let progress_state = state.clone();
        let progress_id = id.clone();
        let mut last = Instant::now();
        // `None` ate o primeiro relatorio: num download retomado o contador ja
        // comeca nos bytes do .part, e partir de zero faria a primeira leitura
        // contar megabytes antigos como se tivessem acabado de chegar.
        let mut last_bytes: Option<u64> = None;
        let cookie = state.cookie_for(Source::Tiktok);

        let on_progress = move |downloaded: u64, total: u64| {
                let now = Instant::now();
                let dt = now.duration_since(last).as_secs_f64();
                // Recalcula a velocidade ~3x/s: suficiente para a UI, barato para o disco.
                if dt >= 0.3 {
                    let sample = last_bytes.map(|prev| (downloaded.saturating_sub(prev)) as f64 / dt);
                    last = now;
                    last_bytes = Some(downloaded);
                    let mut tasks = progress_state.tasks.lock().unwrap();
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == progress_id) {
                        t.downloaded = downloaded;
                        if total > 0 {
                            t.size = total;
                        }
                        if let Some(sample) = sample {
                            // Media exponencial: a janela crua de 0,3s oscila
                            // demais e faz o tempo restante dancar na tela.
                            // Ainda cai a zero rapido quando a conexao para.
                            t.speed = if t.speed > 0.0 {
                                t.speed * 0.4 + sample * 0.6
                            } else {
                                sample
                            };
                        }
                    }
                }
        };

        // Duas midias, dois jeitos de baixar. No TikTok `urls` sao CDNs
        // alternativos do MESMO arquivo (tenta um ate funcionar); no ReelShort
        // sao os segmentos HLS, todos necessarios e em ordem. Mandar um pelo
        // caminho do outro geraria arquivo truncado sem erro nenhum.
        let result = match source {
            Source::Reelshort => {
                let rs_cookie = state.cookie_for(Source::Reelshort);
                download::download_hls(
                    &state.client,
                    &gear.urls,
                    &dest,
                    &rs_cookie,
                    control,
                    on_progress,
                )
                .await
            }
            Source::Tiktok => {
                download::download_episode(
                    &state.client,
                    &gear,
                    &dest,
                    &cookie,
                    control,
                    on_progress,
                )
                .await
            }
        };

        match result {
            Ok(Outcome::Done(n)) => {
                {
                    let mut tasks = state.tasks.lock().unwrap();
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                        t.downloaded = n;
                        t.size = n;
                        t.attempts = 0;
                        t.refreshes = 0;
                    }
                }
                finish(&state, &id, Status::Done, None);
            }
            Ok(Outcome::Paused) => finish(&state, &id, Status::Paused, None),
            Ok(Outcome::Canceled) => {
                // Cancelar remove a tarefa da lista; o .part fica para retomada futura.
                state.tasks.lock().unwrap().retain(|t| t.id != id);
                state.controls.lock().unwrap().remove(&id);
                state.mark_dirty();
                save(&app);
            }
            Err(e) => {
                let msg = e.to_string();
                // Diagnostico: grava a falha completa (URL + resposta do CDN) num
                // arquivo que pode ser inspecionado. So mantem a ultima falha.
                if let Ok(dir) = app.path().app_data_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(
                        dir.join("diag.log"),
                        format!(
                            "tarefa {id}\ngear {}\nsize {}\n---\n{msg}\n",
                            gear.gear_name, gear.size
                        ),
                    );
                }
                // URL vencida/morta entre a listagem e o download, ou reassinada
                // para outra variante: nao adianta insistir no mesmo link, tem
                // que reconsultar a serie. Qualquer outra falha (rede caiu, CDN
                // travou) e transitoria e so pede uma nova tentativa.
                let stale = msg.contains("403")
                    || msg.contains("401")
                    || msg.contains("404")
                    || msg.contains("tamanho inesperado");
                let reason = friendly(&msg);
                if !schedule_retry(&state, &id, &reason, stale) {
                    finish(&state, &id, Status::Failed, Some(reason));
                }
            }
        }
    });
}

fn finish(state: &Arc<AppState>, id: &str, status: Status, error: Option<String>) {
    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        if let Some(start) = t.active_since.take() {
            t.elapsed_secs += now_secs().saturating_sub(start);
        }
        t.status = status;
        t.error = error;
        t.speed = 0.0;
        t.retry_at = 0;
        t.finished_at = Some(now_secs());
    }
    drop(tasks);
    state.mark_dirty();
}

/// Sessao morta. Nao adianta insistir: repetir seis vezes com espera so atrasa
/// a unica coisa que resolve, que e o usuario entrar no TikTok de novo.
fn is_auth_error(msg: &str) -> bool {
    msg.contains(tiktok::AUTH_HINT)
}

/// Traduz o erro cru (dump de HTTP, erro do reqwest) para uma frase curta que
/// cabe na linha da fila. O texto completo continua indo para o diag.log.
fn friendly(msg: &str) -> String {
    let m = msg.to_lowercase();
    if is_auth_error(msg) {
        "a sessao do TikTok expirou — clique em Entrar"
    } else if m.contains("403") || m.contains("401") {
        "o link expirou"
    } else if m.contains("404") {
        "o arquivo saiu do ar"
    } else if m.contains("tamanho inesperado") {
        "o link mudou de versao"
    } else if m.contains("travou") || m.contains("timed out") || m.contains("timeout") {
        "a conexao travou"
    } else if m.contains("incompleto") {
        "a transferencia veio incompleta"
    } else if m.contains("dns") || m.contains("connect") || m.contains("sending request") {
        "sem conexao com a internet"
    } else if m.contains("espaco") || m.contains("os error 112") {
        "acabou o espaco em disco"
    } else if m.contains("nao existe mais") {
        "o episodio saiu do ar"
    } else if m.contains("sem midia acessivel") {
        "precisa ser desbloqueado no TikTok"
    } else {
        "falha de rede"
    }
    .to_string()
}

/// Reagenda a tarefa para uma nova tentativa automatica com espera crescente.
///
/// Devolve `false` quando as tentativas acabaram — ai sim a tarefa falha de
/// vez. E isto que permite deixar a fila baixando durante a noite: uma queda
/// de rede vira uma espera, nao uma fila morta esperando um clique.
fn schedule_retry(state: &Arc<AppState>, id: &str, reason: &str, renew_urls: bool) -> bool {
    let mut tasks = state.tasks.lock().unwrap();
    let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
        return false;
    };
    // Pausar/cancelar durante a espera vence a nova tentativa.
    if matches!(t.status, Status::Paused | Status::Done) {
        return true;
    }
    if t.attempts + 1 >= MAX_ATTEMPTS {
        return false;
    }
    t.attempts += 1;
    if let Some(start) = t.active_since.take() {
        t.elapsed_secs += now_secs().saturating_sub(start);
    }
    if renew_urls {
        t.refreshes = t.refreshes.saturating_add(1);
        t.urls.clear();
    }
    let wait = backoff_secs(t.attempts);
    t.status = Status::Queued;
    t.speed = 0.0;
    t.retry_at = now_secs() + wait;
    t.error = Some(format!(
        "{reason} — tentando de novo ({}/{})",
        t.attempts + 1,
        MAX_ATTEMPTS
    ));
    drop(tasks);
    state.mark_dirty();
    true
}

// ------------------------------------------------------------------- manipulacao

/// Id unico da tarefa. O prefixo da fonte impede que um `book_id` do ReelShort
/// e um `dramaID` do TikTok sejam confundidos como a mesma tarefa.
pub fn task_id(source: Source, drama_id: &str, episode: u32) -> String {
    format!("{}:{drama_id}-{episode}", source.tag())
}

/// Adiciona episodios a fila, ignorando os que ja estao la.
pub fn enqueue(
    state: &Arc<AppState>,
    series: &Series,
    episodes: &[u32],
    dest_dir: &str,
    priority: u8,
) -> usize {
    let (codec, gear_override) = {
        let s = state.settings.lock().unwrap();
        (s.codec.clone(), s.gear.clone())
    };
    let safe = media::safe_name(&series.name);
    let dir = PathBuf::from(dest_dir).join(&safe);
    let ext = series.extension();
    let mut added = 0;

    let mut tasks = state.tasks.lock().unwrap();
    for num in episodes {
        let id = task_id(series.source, &series.drama_id, *num);
        if tasks.iter().any(|t| t.id == id) {
            continue;
        }
        let Some(ep) = series.episodes.iter().find(|e| e.number == *num) else {
            continue;
        };
        // Uma unica escolha de gear por episodio: chamar pick_gear tres vezes
        // podia devolver variantes diferentes e cruzar tamanho com URL.
        let gear = media::pick_gear(ep, &codec, gear_override.as_deref());
        let dest = dir.join(format!("{safe} - E{:02}.{ext}", num));
        let size = gear.as_ref().map(|g| g.size).unwrap_or(0);

        // No ReelShort o tamanho so se conhece baixando, entao "ja tenho" nao
        // pode ser por tamanho: vale o arquivo final existir com conteudo.
        let have_file = if size > 0 {
            download::already_complete(&dest, size)
        } else {
            std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0
        };

        let (status, error) = if !ep.available() {
            // Episodio pago entra na lista para o usuario ver que existe, mas
            // nunca vira download; sem midia, idem, por outro motivo.
            let motivo = if ep.locked {
                ep.lock_reason.clone()
            } else {
                "sem midia acessivel".to_string()
            };
            (Status::Blocked, Some(motivo))
        } else if have_file {
            (Status::Done, None)
        } else {
            (Status::Queued, None)
        };

        // Progresso real desde o primeiro instante: o arquivo pronto, ou o que
        // ja existe de .part de uma sessao anterior. Comecar em zero seria
        // mentir sobre bytes que ja estao no disco.
        let mut size = size;
        let downloaded = if status == Status::Done {
            let actual = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(size);
            // Fonte sem tamanho anunciado: o arquivo no disco e a verdade.
            if size == 0 {
                size = actual;
            }
            actual
        } else {
            let part = std::fs::metadata(download::part_path(&dest))
                .map(|m| m.len())
                .unwrap_or(0);
            // Um .part maior que o esperado e lixo: nao pode virar 110% na barra.
            if size > 0 { part.min(size) } else { part }
        };

        tasks.push(Task {
            id,
            drama_id: series.drama_id.clone(),
            source: series.source,
            item_id: ep.item_id.clone(),
            lang: series.lang.clone(),
            locked: ep.locked,
            series: series.name.clone(),
            episode: *num,
            dest: dest.to_string_lossy().to_string(),
            gear_name: gear.as_ref().map(|g| g.gear_name.clone()).unwrap_or_default(),
            codec: gear
                .as_ref()
                .map(|g| g.codec.clone())
                .or_else(|| ep.gears.first().map(|g| g.codec.clone()))
                .unwrap_or_default(),
            width: gear
                .as_ref()
                .map(|g| g.width)
                .or_else(|| ep.gears.first().map(|g| g.width))
                .unwrap_or(0),
            height: gear
                .as_ref()
                .map(|g| g.height)
                .or_else(|| ep.gears.first().map(|g| g.height))
                .unwrap_or(0),
            duration: ep.duration,
            size,
            downloaded,
            status,
            priority,
            error,
            seq: state.seq.fetch_add(1, Ordering::Relaxed),
            refreshes: 0,
            urls: gear.map(|g| g.urls.clone()).unwrap_or_default(),
            speed: 0.0,
            attempts: 0,
            retry_at: 0,
            elapsed_secs: 0,
            created_at: now_secs(),
            finished_at: if status == Status::Done {
                Some(now_secs())
            } else {
                None
            },
            active_since: None,
        });
        added += 1;
    }
    drop(tasks);
    state.mark_dirty();
    added
}

pub fn pause(state: &Arc<AppState>, id: &str) {
    state.control_for(id).store(PAUSE, Ordering::Relaxed);
    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        // Ainda na fila: pausa direto, sem passar por Running.
        if t.status == Status::Queued {
            t.status = Status::Paused;
        }
    }
    drop(tasks);
    state.mark_dirty();
}

pub fn resume(state: &Arc<AppState>, id: &str) {
    state.control_for(id).store(RUN, Ordering::Relaxed);
    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        if matches!(t.status, Status::Paused | Status::Failed) {
            t.status = Status::Queued;
            t.error = None;
            // Retomar e um pedido explicito: devolve o orcamento de tentativas
            // e dispensa a espera do backoff.
            t.attempts = 0;
            t.refreshes = 0;
            t.retry_at = 0;
            t.finished_at = None;
        }
    }
    drop(tasks);
    state.mark_dirty();
}

pub fn cancel(state: &Arc<AppState>, id: &str, delete_file: bool) {
    state.control_for(id).store(CANCEL, Ordering::Relaxed);
    let dest = {
        let tasks = state.tasks.lock().unwrap();
        tasks.iter().find(|t| t.id == id).map(|t| t.dest.clone())
    };
    let was_running = {
        let tasks = state.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.status == Status::Running)
            .unwrap_or(false)
    };
    // Se nao esta rodando, ninguem vai observar o sinal: remove aqui mesmo.
    if !was_running {
        state.tasks.lock().unwrap().retain(|t| t.id != id);
        state.controls.lock().unwrap().remove(id);
    }
    if delete_file {
        if let Some(d) = dest {
            let p = PathBuf::from(&d);
            // Inclui a anotacao de retomada do HLS: deixar o .idx para tras
            // sujaria a pasta com lixo de um download que nao existe mais.
            download::remove_artifacts(&p);
            let _ = std::fs::remove_file(&p);
        }
    }
    state.mark_dirty();
}

pub fn set_priority(state: &Arc<AppState>, id: &str, priority: u8) {
    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        t.priority = priority.min(PRIO_HIGH);
    }
    drop(tasks);
    state.mark_dirty();
}

pub fn clear_finished(state: &Arc<AppState>) {
    state
        .tasks
        .lock()
        .unwrap()
        .retain(|t| !matches!(t.status, Status::Done | Status::Blocked));
    state.mark_dirty();
}

pub fn retry_failed(state: &Arc<AppState>) {
    let mut tasks = state.tasks.lock().unwrap();
    for t in tasks.iter_mut() {
        if t.status == Status::Failed {
            t.status = Status::Queued;
            t.error = None;
            t.urls.clear();
            t.attempts = 0;
            t.refreshes = 0;
            t.retry_at = 0;
            t.finished_at = None;
        }
    }
    drop(tasks);
    state.mark_dirty();
}

/// Bytes ainda faltando para concluir a fila (usado no aviso de espaco em disco).
pub fn remaining_bytes(state: &Arc<AppState>) -> u64 {
    state
        .tasks
        .lock()
        .unwrap()
        .iter()
        .filter(|t| matches!(t.status, Status::Queued | Status::Running | Status::Paused))
        .map(|t| t.size.saturating_sub(t.downloaded))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: Status, priority: u8, seq: u64) -> Task {
        Task {
            id: id.into(),
            drama_id: "d".into(),
            source: Source::Tiktok,
            item_id: "i".into(),
            lang: String::new(),
            locked: false,
            series: "s".into(),
            episode: 1,
            dest: "x.mp4".into(),
            gear_name: String::new(),
            codec: String::new(),
            width: 0,
            height: 0,
            duration: 0,
            size: 100,
            downloaded: 20,
            status,
            priority,
            error: None,
            seq,
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
    fn prioridade_alta_vem_antes_mesmo_entrando_depois() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(task("a", Status::Queued, PRIO_NORMAL, 1));
            t.push(task("b", Status::Queued, PRIO_HIGH, 2));
            t.push(task("c", Status::Queued, PRIO_HIGH, 3));
        }
        // "b" e "c" sao alta; entre elas vence a que entrou primeiro.
        assert_eq!(pick_next(&state).as_deref(), Some("b"));
    }

    #[test]
    fn empate_de_prioridade_respeita_ordem_de_entrada() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(task("segunda", Status::Queued, PRIO_NORMAL, 5));
            t.push(task("primeira", Status::Queued, PRIO_NORMAL, 2));
        }
        assert_eq!(pick_next(&state).as_deref(), Some("primeira"));
    }

    #[test]
    fn so_considera_tarefas_na_fila() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(task("rodando", Status::Running, PRIO_HIGH, 1));
            t.push(task("pausada", Status::Paused, PRIO_HIGH, 2));
            t.push(task("pronta", Status::Done, PRIO_HIGH, 3));
            t.push(task("na_fila", Status::Queued, PRIO_LOW, 4));
        }
        assert_eq!(pick_next(&state).as_deref(), Some("na_fila"));
        assert_eq!(running_count(&state), 1);
    }

    #[test]
    fn fila_vazia_nao_escolhe_nada() {
        let state = Arc::new(AppState::new());
        assert_eq!(pick_next(&state), None);
    }

    /// Contrato com a interface. Um `#[serde(skip)]` num campo lido pela tela
    /// nao quebra a compilacao: o valor simplesmente chega `undefined` e vira
    /// conta com NaN. Foi assim que o tempo restante virou "NaNhNaN" na tela e
    /// as colunas de velocidade ficaram vazias. Este teste tranca a porta.
    #[test]
    fn payload_da_tarefa_leva_todo_campo_que_a_tela_le() {
        let mut t = task("a", Status::Running, PRIO_NORMAL, 1);
        t.speed = 1234.5;
        let json = serde_json::to_value(&t).expect("serializa");
        let obj = json.as_object().expect("objeto");

        for campo in [
            "id", "series", "episode", "dest", "gear_name", "width", "height", "size",
            "downloaded", "status", "priority", "error", "seq", "speed", "attempts",
            "retry_at", "created_at", "finished_at",
        ] {
            assert!(obj.contains_key(campo), "campo `{campo}` sumiu do payload");
        }
        // E precisa chegar com o valor real, nao com o default.
        assert_eq!(obj["speed"], serde_json::json!(1234.5));
    }

    /// O paywall precisa morar na tarefa, nao no status: quando quem baixa roda,
    /// o agendador ja trocou o status para `Running`. Inferir dali deixaria a
    /// barreira sempre desligada, sem ninguem perceber.
    #[test]
    fn tarefa_trancada_continua_trancada_mesmo_rodando() {
        let mut t = task("a", Status::Running, PRIO_NORMAL, 1);
        t.locked = true;
        assert_ne!(t.status, Status::Blocked);
        assert!(t.locked, "o cadeado nao pode depender do status");
    }

    /// Estado salvo antes das duas fontes usava id sem prefixo de fonte.
    #[test]
    fn id_antigo_ganha_prefixo_para_nao_duplicar_tarefa() {
        let antigo = "7677784436146164743-3";
        assert!(!antigo.contains(':'));
        let novo = task_id(Source::Tiktok, "7677784436146164743", 3);
        assert_eq!(novo, "tk:7677784436146164743-3");
        // O que o enqueue procura tem que ser o que o load deixou.
        assert_eq!(novo, format!("tk:{antigo}"));
        // E as duas fontes nunca colidem no mesmo episodio.
        assert_ne!(novo, task_id(Source::Reelshort, "7677784436146164743", 3));
    }

    #[test]
    fn tarefa_em_backoff_nao_e_escolhida_antes_da_hora() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            let mut esperando = task("esperando", Status::Queued, PRIO_HIGH, 1);
            esperando.retry_at = now_secs() + 60;
            t.push(esperando);
            t.push(task("pronta", Status::Queued, PRIO_LOW, 2));
        }
        // A de alta prioridade esta cumprindo backoff: a fila segue sem ela.
        assert_eq!(pick_next(&state).as_deref(), Some("pronta"));
        assert!(has_pending_retry(&state));
    }

    #[test]
    fn backoff_vencido_volta_a_ser_elegivel() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            let mut vencida = task("vencida", Status::Queued, PRIO_NORMAL, 1);
            vencida.retry_at = now_secs() - 1;
            t.push(vencida);
        }
        assert_eq!(pick_next(&state).as_deref(), Some("vencida"));
        assert!(!has_pending_retry(&state));
    }

    #[test]
    fn falha_transitoria_reagenda_ate_o_limite_e_entao_desiste() {
        let state = Arc::new(AppState::new());
        state
            .tasks
            .lock()
            .unwrap()
            .push(task("a", Status::Running, PRIO_NORMAL, 1));

        // As primeiras falhas viram espera, nao morte da tarefa.
        for esperada in 1..MAX_ATTEMPTS {
            assert!(
                schedule_retry(&state, "a", "sem conexao com a internet", false),
                "tentativa {esperada} deveria reagendar"
            );
            let tasks = state.tasks.lock().unwrap();
            let t = &tasks[0];
            assert_eq!(t.status, Status::Queued);
            assert_eq!(t.attempts, esperada);
            assert!(t.retry_at > now_secs(), "precisa esperar antes de repetir");
            assert!(t.error.as_deref().unwrap().contains("tentando de novo"));
        }
        // Esgotado o orcamento, a proxima falha e definitiva.
        assert!(!schedule_retry(&state, "a", "sem conexao com a internet", false));
    }

    #[test]
    fn retomar_devolve_o_orcamento_de_tentativas() {
        let state = Arc::new(AppState::new());
        {
            let mut tasks = state.tasks.lock().unwrap();
            let mut t = task("a", Status::Failed, PRIO_NORMAL, 1);
            t.attempts = MAX_ATTEMPTS;
            t.retry_at = now_secs() + 999;
            tasks.push(t);
        }
        resume(&state, "a");
        let tasks = state.tasks.lock().unwrap();
        assert_eq!(tasks[0].status, Status::Queued);
        assert_eq!(tasks[0].attempts, 0);
        assert_eq!(tasks[0].retry_at, 0);
    }

    #[test]
    fn espera_cresce_mas_tem_teto() {
        assert!(backoff_secs(1) < backoff_secs(3));
        assert!(backoff_secs(3) < backoff_secs(5));
        assert_eq!(backoff_secs(9), backoff_secs(20));
    }

    #[test]
    fn erro_cru_vira_frase_curta() {
        assert_eq!(
            friendly("HTTP 403 | ct=text/html | body=... | url=https://v16.tiktok..."),
            "o link expirou"
        );
        assert_eq!(friendly("conexao travou (60s sem dados)"), "a conexao travou");
        // Sessao morta precisa apontar para o login, nunca para "falha de rede".
        let auth = format!("API respondeu HTTP 403. {}", tiktok::AUTH_HINT);
        assert!(is_auth_error(&auth));
        assert_eq!(friendly(&auth), "a sessao do TikTok expirou — clique em Entrar");
        assert_eq!(friendly("tamanho inesperado (10 != 20)"), "o link mudou de versao");
        // O que nao se reconhece ainda sai legivel, nunca como dump.
        assert_eq!(friendly("algo muito estranho"), "falha de rede");
    }

    #[test]
    fn bytes_restantes_ignora_concluidas() {
        let state = Arc::new(AppState::new());
        {
            let mut t = state.tasks.lock().unwrap();
            t.push(task("a", Status::Queued, PRIO_NORMAL, 1)); // 100-20 = 80
            t.push(task("b", Status::Running, PRIO_NORMAL, 2)); // 80
            t.push(task("c", Status::Done, PRIO_NORMAL, 3)); // ignorada
            t.push(task("d", Status::Blocked, PRIO_NORMAL, 4)); // ignorada
        }
        assert_eq!(remaining_bytes(&state), 160);
    }
}
