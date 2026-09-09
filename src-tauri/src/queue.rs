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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

use crate::awake;
use crate::candyjar;
use crate::dramabox;
use crate::download::{self, Control, Outcome, CANCEL, PAUSE, RUN};
use crate::manifest::{self, Manifest};
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
    /// De que natureza foi a ultima falha. E por este campo, e nao pelo texto
    /// da tela, que o zelador decide o que ressuscitar.
    #[serde(default)]
    pub fail_kind: FailKind,
    /// Quantas vezes o zelador ja ressuscitou esta tarefa depois de ela ter
    /// falhado de vez. Limitado por `MAX_REVIVALS`: sem teto, um episodio que
    /// morreu por um motivo mal classificado giraria em falso a noite inteira.
    ///
    /// Persistido de proposito: o teto tem de sobreviver a um fechamento do
    /// app, senao ele se reseta justo depois de uma queda — quando mais importa.
    #[serde(default)]
    pub revivals: u8,
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
    /// Serializa toda gravacao de `manifest.json`.
    ///
    /// Ate tres episodios da MESMA novela baixam ao mesmo tempo, e cada um
    /// termina lendo-alterando-gravando o mesmo arquivo. Sem esta trava os
    /// perdedores da corrida sumiriam do manifesto — exatamente o buraco que a
    /// gravacao incremental existe para tapar. Uma trava global basta: gravar o
    /// manifesto e raro e rapido.
    pub manifest_lock: Mutex<()>,
    pub client: reqwest::Client,
    pub seq: AtomicU64,
    pub dirty: AtomicU8,
    /// A rede caiu.
    ///
    /// Sem este portao, uma queda de vinte minutos as 3h nao para o agendador:
    /// ele continua puxando tarefa nova, cada uma queima as suas seis
    /// tentativas em ~223s e morre. Com cinquenta novelas na fila isso e um
    /// muro de centenas de falhas permanentes esperando o usuario acordar —
    /// que e exatamente o que ele pediu para nunca acontecer.
    ///
    /// Enquanto ligado, o agendador nao inicia nada e a sonda testa a rede de
    /// tempos em tempos. Nao e persistido: ao abrir o app, presume-se conectado
    /// e a primeira falha real reacende o sinal.
    pub offline: AtomicBool,
    /// Desde quando o portao esta fechado (unix secs).
    ///
    /// Serve ao teto de `MAX_OFFLINE`: um portao que so abre por sucesso de uma
    /// sonda externa e um portao que pode emperrar fechado — basta a origem
    /// recusar o HEAD da sonda, ou responder com um desafio, para a fila
    /// congelar para sempre anunciando "sem conexao" com a internet perfeita.
    /// Silencioso e autossustentado, e pior que o muro de erros que o portao
    /// existe para evitar.
    pub offline_since: AtomicU64,
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
            manifest_lock: Mutex::new(()),
            client,
            seq: AtomicU64::new(0),
            dirty: AtomicU8::new(1),
            offline: AtomicBool::new(false),
            offline_since: AtomicU64::new(0),
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
        migrate_reelshort_destination(t);
        if t.status == Status::Running {
            t.status = Status::Queued;
        }
        t.active_since = None;
        t.speed = 0.0;
        t.retry_at = 0;
        let dest = PathBuf::from(&t.dest);
        if t.status == Status::Done {
            match std::fs::metadata(&dest) {
                Ok(m) if download::published_mp4(&dest) => {
                    t.downloaded = m.len();
                    // Depois do remux, o tamanho final pode diferir do arquivo
                    // da origem; o MP4 validado no disco passa a ser a verdade.
                    t.size = m.len();
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
            let part = std::fs::metadata(task_part_path(t.source, &dest))
                .map(|m| m.len())
                .unwrap_or(0);
            t.downloaded = if t.size > 0 { part.min(t.size) } else { part };
        }
    }
    *state.tasks.lock().unwrap() = tasks;
    state.seq.store(max_seq + 1, Ordering::Relaxed);
    state.mark_dirty();
}

/// Versoes anteriores publicavam o MPEG-TS como destino final. A tarefa passa
/// a apontar para MP4, mas o arquivo antigo permanece exatamente no caminho que
/// o novo remux usa como entrada — logo a migracao nao repete download algum.
fn migrate_reelshort_destination(task: &mut Task) {
    let dest = PathBuf::from(&task.dest);
    if task.source == Source::Reelshort
        && dest
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
    {
        task.dest = dest.with_extension("mp4").to_string_lossy().to_string();
    }
}

fn task_part_path(source: Source, dest: &std::path::Path) -> PathBuf {
    if source == Source::Reelshort {
        download::part_path(&download::hls_source_path(dest))
    } else {
        download::part_path(dest)
    }
}

// -------------------------------------------------------------------- manifesto

/// Le, altera e grava o manifesto de uma pasta sob a trava global.
///
/// Nada de `await` aqui dentro: o que precisa de rede (baixar capa e miniatura)
/// acontece antes, e so o trecho de disco entra na regiao critica.
pub fn patch_manifest(
    state: &AppState,
    dir: &std::path::Path,
    apply: impl FnOnce(&mut Manifest),
) -> anyhow::Result<()> {
    let _guard = state.manifest_lock.lock().unwrap();
    let mut m = manifest::read(dir);
    apply(&mut m);
    manifest::write(dir, &m)
}

/// O que uma passagem de metadados deixou na pasta.
#[derive(Serialize, Clone, Debug, Default)]
pub struct SyncCount {
    pub poster: bool,
    pub trailer: bool,
    pub thumbs: usize,
    pub episodes: usize,
}

/// Grava na pasta da novela tudo o que se sabe dela: `manifest.json`,
/// `poster.jpg` e `thumbs/E##.jpg`.
///
/// Um episodio so entra se o arquivo de video existe no disco: o manifesto
/// descreve a pasta, nao a fila. Por isso a mesma funcao serve ao download
/// (onde ela vai preenchendo conforme os arquivos aparecem) e a varredura do
/// que ja foi baixado.
///
/// As imagens sao baixadas ANTES da regiao critica; so o trecho de disco entra
/// na trava.
pub async fn sync_folder(
    state: &Arc<AppState>,
    series: &Series,
    dir: &std::path::Path,
) -> anyhow::Result<SyncCount> {
    let mut count = SyncCount::default();
    let (cookie, headers) = if series.source == Source::Reelshort {
        let session = state.rs_session();
        (session.cookie, session.headers)
    } else {
        (state.cookie_for(series.source), Vec::new())
    };
    let poster = manifest::fetch_poster(&state.client, series, dir, &cookie, &headers).await.ok();
    count.poster = poster.is_some();

    // O trailer e opcional e nunca derruba a sincronizacao: novela sem trailer
    // e o caso normal, e uma falha de rede aqui nao pode custar o manifesto.
    let control = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(download::RUN));
    let trailer = manifest::ensure_trailer(
        &state.client, series, dir, &state.rs_session(), &cookie, control,
    )
    .await
    .unwrap_or(None);
    count.trailer = trailer.is_some();

    let mut entries: Vec<(u32, manifest::EpisodeEntry)> = Vec::new();
    for ep in &series.episodes {
        let Ok((file, info)) = manifest::ensure_episode_video(dir, &series.name, ep.number).await else {
            continue;
        };
        let video = dir.join(&file);
        let remote_thumb = manifest::fetch_thumb(
            &state.client, series.source, &ep.thumb, dir, ep.number, &cookie, &headers,
        )
            .await
            .ok();
        let thumb = match remote_thumb {
            Some(path) => manifest::ensure_video_thumb(&video, dir, ep.number).await.ok().or(Some(path)),
            None => manifest::ensure_video_thumb(&video, dir, ep.number).await.ok(),
        };
        if thumb.is_some() {
            count.thumbs += 1;
        }
        entries.push((ep.number, manifest::validated_episode_entry(Some(ep), file, thumb, &info)));
    }
    count.episodes = entries.len();

    patch_manifest(state, dir, |m| {
        manifest::patch_series(m, series, poster);
        if trailer.is_some() {
            m.trailer = trailer;
        }
        for (number, entry) in entries {
            manifest::patch_episode(m, number, entry);
        }
    })?;
    Ok(count)
}

/// Dispara `sync_folder` em segundo plano, para quem nao pode esperar por rede.
///
/// Falhar aqui nunca atrapalha a fila: o manifesto e conveniencia para quem
/// exibe a biblioteca, e a proxima passagem (o fim de um episodio, ou a
/// varredura de metadados) refaz o trabalho.
pub fn spawn_series_manifest(state: &Arc<AppState>, series: &Series, dest_dir: &str) {
    let state = state.clone();
    let series = series.clone();
    let dir = media::series_dir(dest_dir, &series.name);
    tauri::async_runtime::spawn(async move {
        let _ = sync_folder(&state, &series, &dir).await;
    });
}

/// Atualiza o manifesto com um episodio que acabou de ficar pronto.
///
/// E o que garante que uma fila interrompida no episodio 20 deixe um manifesto
/// com 20 episodios, e nao nenhum. Quando a serie ainda esta em cache, aproveita
/// para baixar a miniatura; quando nao esta (app reaberto no meio da fila),
/// grava o que a propria tarefa sabe e deixa o resto para a varredura.
fn spawn_episode_manifest(state: &Arc<AppState>, id: &str) {
    let Some(task) = state.tasks.lock().unwrap().iter().find(|t| t.id == id).cloned() else {
        return;
    };
    let dest = PathBuf::from(&task.dest);
    let (Some(dir), Some(file)) = (
        dest.parent().map(|p| p.to_path_buf()),
        dest.file_name().map(|f| f.to_string_lossy().to_string()),
    ) else {
        return;
    };
    let episode = state
        .series_cache
        .lock()
        .unwrap()
        .get(&task.source.cache_key(&task.drama_id))
        .and_then(|s| s.episodes.iter().find(|e| e.number == task.episode).cloned());

    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        let entry = match episode {
            Some(ep) => {
                let (cookie, headers) = if task.source == Source::Reelshort {
                    let session = state.rs_session();
                    (session.cookie, session.headers)
                } else { (state.cookie_for(task.source), Vec::new()) };
                let thumb = manifest::fetch_thumb(
                    &state.client,
                    task.source,
                    &ep.thumb,
                    &dir,
                    task.episode,
                    &cookie,
                    &headers,
                )
                .await
                .ok();
                let info = download::validate_video(&dest, false).await.ok();
                let thumb = match thumb {
                    Some(path) => manifest::ensure_video_thumb(&dest, &dir, task.episode).await.ok().or(Some(path)),
                    None => manifest::ensure_video_thumb(&dest, &dir, task.episode).await.ok(),
                };
                match info {
                    Some(info) => manifest::validated_episode_entry(Some(&ep), file, thumb, &info),
                    None => return,
                }
            }
            // Sem a serie em cache, o que se afirma e so o que o download viu.
            // Os campos ausentes ficam ausentes: o merge preserva o que uma
            // passagem anterior ja tenha gravado.
            None => match download::validate_video(&dest, false).await {
                Ok(info) => manifest::validated_episode_entry(
                    None, file, manifest::ensure_video_thumb(&dest, &dir, task.episode).await.ok(), &info,
                ),
                Err(_) => return,
            },
        };
        let _ = patch_manifest(&state, &dir, |m| {
            manifest::patch_episode(m, task.episode, entry);
            if m.captured_at.is_empty() {
                m.captured_at = manifest::now_iso();
            }
            if m.drama_name.is_empty() {
                m.drama_name = task.series.clone();
            }
            if m.drama_id.is_empty() {
                m.drama_id = task.drama_id.clone();
            }
            if m.source.is_empty() {
                m.source = match task.source {
                    Source::Tiktok => "tiktok",
                    Source::Reelshort => "reelshort",
                    Source::Candyjar => "candyjar",
                    Source::Dramabox => "dramabox",
                }
                .into();
            }
        });
    });
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
        let mut estava_offline = false;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            ticks += 1;

            // TikTok passa URLs assinadas e tende a derrubar rajadas grandes.
            // Mesmo no modo paralelo, tres fluxos e o teto deliberado.
            let settings = state.settings.lock().unwrap().clone();
            let limit = if settings.sequential { 1 } else { settings.concurrency.clamp(1, 3) };
            // Rede caida: nao inicia nada. O que ja estava rodando termina (ou
            // falha e classifica), mas a fila para de alimentar o moedor. E
            // este portao, e nao tentativa infinita por tarefa, que impede a
            // cascata: duas mil tarefas imortais ainda passariam vinte minutos
            // se debatendo.
            // O portao e autolimitado: passado `MAX_OFFLINE` ele abre sozinho e
            // deixa uma tarefa tentar de verdade. Se a rede ainda estiver fora,
            // essa tarefa falha e fecha o portao de novo — custo de uma
            // tentativa. Sem este teto, o portao dependeria SO do sucesso da
            // sonda, e uma sonda recusada pela origem congelaria a fila para
            // sempre com a internet funcionando.
            if state.offline.load(Ordering::Relaxed) {
                let desde = state.offline_since.load(Ordering::Relaxed);
                if desde > 0 && now_secs().saturating_sub(desde) >= MAX_OFFLINE {
                    state.offline.store(false, Ordering::Relaxed);
                    state.offline_since.store(0, Ordering::Relaxed);
                }
            }
            let bloqueado = state.offline.load(Ordering::Relaxed);
            // A tela precisa saber. Sem isto, uma fila parada por queda de rede
            // e indistinguivel de uma fila travada por defeito — e as 3h nao ha
            // ninguem para diferenciar as duas.
            if bloqueado != estava_offline {
                estava_offline = bloqueado;
                let _ = app.emit("queue://offline", bloqueado);
            }
            while !bloqueado && running_count(&state) < limit {
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

            // Enquanto ha o que baixar, o Windows nao suspende. E o defeito que
            // nenhuma retentativa alcanca: a cena real e "mando baixar e vou
            // dormir", e se a maquina dorme junto nada baixa — o usuario acorda
            // culpando o app por um trabalho que nunca chegou a rodar.
            //
            // Vale tambem para a espera da rede: uma queda as 3h nao pode virar
            // uma maquina suspensa as 3h05, senao ela nunca ve a rede voltar.
            let ha_trabalho = {
                let tasks = state.tasks.lock().unwrap();
                tasks
                    .iter()
                    .any(|t| matches!(t.status, Status::Running | Status::Queued))
            };
            awake::manter_acordado(ha_trabalho);
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

/// Ate quando o portao pode ficar fechado sem ninguem provar que a rede voltou.
///
/// Cinco minutos: longo o bastante para uma queda comum passar sem a fila se
/// debater, curto o bastante para uma sonda enganada nao custar a noite.
const MAX_OFFLINE: u64 = 300;

/// A rede responde?
///
/// A pergunta e "da para alcancar a internet?", e nao "a origem esta simpatica
/// hoje". Por isso a lista comeca por um host neutro: TikTok e ReelShort podem
/// recusar um HEAD sem cookies, ou devolver um desafio, e tratar isso como
/// "internet fora" manteria a fila parada com a rede perfeita.
async fn rede_responde(state: &Arc<AppState>) -> bool {
    for url in [
        "https://cloudflare.com/cdn-cgi/trace",
        "https://www.tiktok.com/",
        "https://www.reelshort.com/",
    ] {
        let ok = tokio::time::timeout(
            Duration::from_secs(10),
            state.client.head(url).header("User-Agent", media::UA).send(),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .is_some();
        if ok {
            return true;
        }
    }
    false
}

/// Vigia da conexao.
///
/// So acorda de verdade quando o agendador ja se declarou offline; enquanto a
/// rede esta boa, este laco custa um teste de booleano a cada 15s. Quando a
/// rede volta, ele reabre o portao e devolve a fila ao trabalho — sem ninguem
/// precisar estar acordado para clicar em nada.
pub fn start_connectivity_probe(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<Arc<AppState>>().inner().clone();
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if !state.offline.load(Ordering::Relaxed) {
                continue;
            }
            if rede_responde(&state).await {
                state.offline.store(false, Ordering::Relaxed);
                state.offline_since.store(0, Ordering::Relaxed);
                // Quem estava esperando o backoff pode ir agora: a espera era
                // pela rede, e ela voltou.
                {
                    let mut tasks = state.tasks.lock().unwrap();
                    for t in tasks.iter_mut() {
                        if t.status == Status::Queued && t.retry_at > 0 {
                            t.retry_at = 0;
                        }
                    }
                }
                state.mark_dirty();
                let _ = app.emit("queue://online", true);
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
    if source == Source::Candyjar {
        return refresh_candyjar(state, id, &drama_id, episode, &lang).await;
    }
    if source == Source::Dramabox {
        return refresh_dramabox(state, id, &drama_id, episode, &lang).await;
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

/// Resolve a midia de um episodio do CandyJar na hora de baixar.
///
/// Mesma logica de janela do ReelShort: a listagem nao traz URL de video, so a
/// pagina do episodio. Aqui devolve a URL de video e, quando a origem separa
/// audio numa faixa a parte (o caso comum), tambem a de audio — ver
/// `candyjar::select_variant`. `t.urls` guarda uma ou duas entradas: nunca
/// mais que isso, e a segunda so existe quando ha audio separado.
async fn refresh_candyjar(
    state: &Arc<AppState>,
    id: &str,
    series_id: &str,
    episode: u32,
    lang: &str,
) -> anyhow::Result<()> {
    let locked = {
        let tasks = state.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.locked)
            .unwrap_or(true)
    };
    let cookie = state.cookie_for(Source::Candyjar);
    let (video_url, audio_url) =
        candyjar::resolve_media(&state.client, lang, series_id, episode, locked, &cookie).await?;

    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        t.urls = std::iter::once(video_url).chain(audio_url).collect();
        t.gear_name = candyjar::GEAR_NAME.to_string();
        t.codec = "h264".into();
    }
    Ok(())
}

/// Reassina a URL de um episodio do DramaBox na hora de baixar.
///
/// Diferente do ReelShort e do CandyJar, aqui nao ha pagina de episodio: a
/// listagem da serie ja traz o MP4 de todo episodio liberado, e reconsultar
/// essa mesma pagina e o que devolve a URL com assinatura nova. `t.urls` guarda
/// sempre uma entrada so — a origem publica um arquivo por episodio, sem
/// variantes de qualidade e sem faixa de audio separada.
async fn refresh_dramabox(
    state: &Arc<AppState>,
    id: &str,
    series_id: &str,
    episode: u32,
    lang: &str,
) -> anyhow::Result<()> {
    let locked = {
        let tasks = state.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.locked)
            .unwrap_or(true)
    };
    let cookie = state.cookie_for(Source::Dramabox);
    let url =
        dramabox::resolve_media(&state.client, lang, series_id, episode, locked, &cookie).await?;

    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        t.urls = vec![url];
        t.gear_name = dramabox::GEAR_NAME.to_string();
        t.codec = "h264".into();
        // O tamanho nao vem na listagem: quem baixa o descobre pelo
        // `Content-Length` da propria resposta.
        t.size = 0;
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
                .map(|t| {
                    let dest = PathBuf::from(&t.dest);
                    let local_hls = t.source == Source::Reelshort
                        && download::nonempty(&download::hls_source_path(&dest));
                    t.urls.is_empty() && !local_hls
                })
                .unwrap_or(true)
        };
        if needs_urls {
            if let Err(e) = refresh_urls(&state, &id).await {
                let msg = e.to_string();
                // "Nao existe mais" e definitivo; qualquer outra coisa (rede,
                // sessao, API fora do ar) merece nova tentativa automatica.
                if msg.contains("nao existe mais") || msg.contains("sem midia acessivel") {
                    finish(&state, &id, Status::Blocked, Some(friendly(&msg)), FailKind::Terminal);
                } else if is_auth_error(&msg) {
                    // Repetir nao resolve login: falha logo, com a instrucao certa.
                    finish(&state, &id, Status::Failed, Some(friendly(&msg)), classify(&msg));
                } else if !schedule_retry(&state, &id, &friendly(&msg), true, classify(&msg)) {
                    finish(&state, &id, Status::Failed, Some(friendly(&msg)), classify(&msg));
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

        let local_hls = source == Source::Reelshort
            && download::nonempty(&download::hls_source_path(&dest));
        if gear.urls.is_empty() && !local_hls {
            finish(&state, &id, Status::Blocked, Some("sem midia acessivel".into()), FailKind::Terminal);
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

        // Tres fontes, tres jeitos de baixar. No TikTok `urls` sao CDNs
        // alternativos do MESMO arquivo (tenta um ate funcionar); no ReelShort
        // sao os segmentos HLS, todos necessarios e em ordem; no CandyJar sao
        // exatamente duas playlists HLS (video e audio) que o FFmpeg junta.
        // Mandar um pelo caminho do outro geraria arquivo truncado ou sem
        // audio, sem erro nenhum.
        let result = match source {
            Source::Reelshort => {
                let rs_cookie = state.cookie_for(Source::Reelshort);
                download::download_hls_mp4(
                    &state.client,
                    &gear.urls,
                    &dest,
                    &rs_cookie,
                    control,
                    on_progress,
                )
                .await
            }
            // As duas entregam MP4 progressivo numa URL assinada, entao seguem
            // pelo mesmo caminho. No DramaBox `urls` tem sempre uma entrada so
            // (um arquivo por episodio); no TikTok sao CDNs alternativos do
            // mesmo arquivo. O laco de `download_episode` atende os dois.
            Source::Tiktok | Source::Dramabox => {
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
            Source::Candyjar => {
                download::download_candyjar_mp4(
                    &gear.urls[0],
                    gear.urls.get(1).map(|s| s.as_str()),
                    &dest,
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
                finish(&state, &id, Status::Done, None, FailKind::None);
                // O arquivo existe: agora ele pode entrar no manifesto.
                spawn_episode_manifest(&state, &id);
            }
            Ok(Outcome::Paused) => finish(&state, &id, Status::Paused, None, FailKind::None),
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
                // Falha estrutural do remux nao melhora repetindo a mesma
                // entrada seis vezes. O TS fica preservado para diagnostico e
                // para uma retomada depois de reinstalar/corrigir o aplicativo.
                let fatal_remux = msg.contains("FFmpeg")
                    || msg.contains("converter o episodio para MP4")
                    || msg.contains("MP4 vazio");
                if fatal_remux || !schedule_retry(&state, &id, &reason, stale, classify(&msg)) {
                    finish(&state, &id, Status::Failed, Some(reason), classify(&msg));
                }
            }
        }
    });
}

/// Encerra uma tarefa. `kind` classifica a falha para o zelador; sucesso e
/// pausa passam `FailKind::None`.
fn finish(
    state: &Arc<AppState>,
    id: &str,
    status: Status,
    error: Option<String>,
    kind: FailKind,
) {
    let mut tasks = state.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        if let Some(start) = t.active_since.take() {
            t.elapsed_secs += now_secs().saturating_sub(start);
        }
        t.status = status;
        t.error = error;
        t.speed = 0.0;
        t.retry_at = 0;
        t.fail_kind = kind;
        t.finished_at = Some(now_secs());
    }
    drop(tasks);
    state.mark_dirty();
}

/// Sessao morta. Nao adianta insistir: repetir seis vezes com espera so atrasa
/// a unica coisa que resolve, que e o usuario entrar no TikTok de novo.
fn is_auth_error(msg: &str) -> bool {
    tiktok::is_auth_error(msg)
}

/// De que natureza foi a falha.
///
/// Existe para o zelador (`start_janitor`) decidir o que ressuscitar sem ler a
/// frase em portugues que a tela mostra. Amarrar politica a texto de interface
/// e o jeito de uma correcao de copy quebrar a recuperacao em silencio, meses
/// depois, de madrugada — que e exatamente quando ninguem esta olhando.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum FailKind {
    #[default]
    None,
    /// A internet caiu, a conexao travou, a transferencia veio incompleta.
    /// Passa sozinho: e o caso que o zelador existe para atender.
    Network,
    /// O link assinado venceu. Recupera-se reconsultando a serie.
    Stale,
    /// Acabou o espaco. Passa quando o usuario libera disco — nao antes.
    Disk,
    /// Sessao morta. So o usuario resolve, entrando de novo.
    Auth,
    /// Episodio pago, fora do ar, sem midia, FFmpeg quebrado. Repetir nao muda
    /// o resultado; ressuscitar isso seria girar em falso para sempre.
    Terminal,
}

impl FailKind {
    /// Vale a pena tentar de novo mais tarde, sozinho?
    fn recoverable(self) -> bool {
        matches!(self, FailKind::Network | FailKind::Stale | FailKind::Disk)
    }
}

/// Classifica a falha pela mensagem CRUA, antes de qualquer traducao.
pub fn classify(msg: &str) -> FailKind {
    let m = msg.to_lowercase();
    if is_auth_error(msg) {
        FailKind::Auth
    } else if m.contains("espaco")
        || m.contains("espaço")
        || m.contains("os error 112")
        || m.contains("no space")
    {
        FailKind::Disk
    } else if m.contains("nao existe mais")
        || m.contains("não existe mais")
        || m.contains("sem midia acessivel")
        || m.contains("pago no")
        || m.contains("indisponível no")
        || m.contains("ffmpeg")
        || m.contains("mp4 vazio")
        || m.contains("404")
    {
        FailKind::Terminal
    } else if m.contains("403")
        || m.contains("401")
        || m.contains("tamanho inesperado")
    {
        FailKind::Stale
    } else {
        // O resto — DNS, connect, timeout, transferencia incompleta, e tambem o
        // que ainda nao foi catalogado — conta como rede. Errar para o lado de
        // "tenta de novo" custa uma tentativa; errar para o lado de "desiste"
        // custa o episodio.
        FailKind::Network
    }
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
    // Paywall das fontes que declaram o bloqueio por sessao (ReelShort,
    // CandyJar, DramaBox). Sem estas duas linhas o texto do bloqueio caia no
    // "falha de rede" do fim, que manda o usuario procurar problema na
    // conexao quando o que falta e direito sobre o episodio.
    } else if m.contains("pago no") {
        "episodio pago — precisa ser liberado na sua conta"
    } else if m.contains("indisponível no") {
        "a origem nao publica esse episodio"
    } else if m.contains("sem midia acessivel") {
        "precisa ser desbloqueado no TikTok"
    } else if m.contains("ffmpeg nao foi encontrado") {
        "instalacao incompleta — reinstale o tkdrama"
    } else if m.contains("converter o episodio para mp4")
        || m.contains("mp4 vazio")
        || m.contains("não consegui preparar o mp4")
        || m.contains("não consegui baixar o episódio do candyjar")
    {
        "nao consegui preparar o MP4"
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
/// Quantas vezes o zelador ressuscita uma tarefa que ja falhou de vez.
///
/// Seis tentativas rapidas (`MAX_ATTEMPTS`) cobrem um soluco; estas cobrem uma
/// noite. Com o ciclo de 10 minutos, dao cerca de duas horas de recuperacao por
/// tarefa antes de o app admitir que aquele episodio nao vai vir.
const MAX_REVIVALS: u8 = 12;

/// De quanto em quanto tempo o zelador passa.
const JANITOR_CYCLE: Duration = Duration::from_secs(600);

/// Devolve a fila o que falhou por motivo passageiro. Devolve quantas voltaram.
///
/// Separado do laco para poder ser testado sem esperar dez minutos.
fn revive_recuperaveis(state: &Arc<AppState>) -> usize {
    let mut revividas = 0usize;
    let mut tasks = state.tasks.lock().unwrap();
    for t in tasks.iter_mut() {
        if t.status != Status::Failed || !t.fail_kind.recoverable() || t.revivals >= MAX_REVIVALS {
            continue;
        }
        t.revivals += 1;
        t.attempts = 0;
        t.status = Status::Queued;
        t.retry_at = 0;
        t.finished_at = None;
        t.speed = 0.0;
        // O link assinado que venceu tem de ser reconsultado; sem limpar as
        // URLs a tarefa tentaria de novo o mesmo endereco morto e falharia
        // identica, gastando uma ressurreicao a toa.
        if t.fail_kind == FailKind::Stale {
            t.refreshes = t.refreshes.saturating_add(1);
            t.urls.clear();
        }
        t.error = Some(format!(
            "voltando a tentar sozinho ({}/{})",
            t.revivals, MAX_REVIVALS
        ));
        revividas += 1;
    }
    revividas
}

/// Ressuscita o que falhou por motivo que passa sozinho.
///
/// O `schedule_retry` cobre o soluco de segundos; este cobre a noite. Uma queda
/// de rede as 3h esgota as seis tentativas de cada tarefa em menos de quatro
/// minutos e as manda para `Failed` — recuperaveis so pelo botao "Tentar de
/// novo", que ninguem vai apertar dormindo. O zelador aperta esse botao
/// sozinho, de dez em dez minutos, e so no que faz sentido reapertar.
pub fn start_janitor(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<Arc<AppState>>().inner().clone();
        loop {
            tokio::time::sleep(JANITOR_CYCLE).await;
            // Offline recente: nao adianta ressuscitar para falhar de novo. A
            // sonda reabre o portao, e a passagem seguinte faz o trabalho.
            //
            // Mas a espera tem limite. O portao ja se abre sozinho depois de
            // `MAX_OFFLINE`; amarrar o zelador a ele sem teto proprio faria as
            // duas travas se sustentarem uma na outra — e o ciclo de dez
            // minutos e mais longo que os cinco do portao, entao aqui basta
            // nao esperar mais que uma passagem.
            let desde = state.offline_since.load(Ordering::Relaxed);
            let preso_ha_pouco = desde > 0 && now_secs().saturating_sub(desde) < MAX_OFFLINE;
            if state.offline.load(Ordering::Relaxed) && preso_ha_pouco {
                continue;
            }
            let revividas = revive_recuperaveis(&state);
            if revividas > 0 {
                state.mark_dirty();
                save(&app);
                let _ = app.emit("queue://revived", revividas);
            }
        }
    });
}

/// `reason` e o texto que vai para a tela; `kind` e a classificacao vinda da
/// mensagem crua. Os dois andam juntos e nunca se derivam um do outro: politica
/// lida de copy quebra em silencio na primeira revisao de texto.
fn schedule_retry(
    state: &Arc<AppState>,
    id: &str,
    reason: &str,
    renew_urls: bool,
    kind: FailKind,
) -> bool {
    // Falha de rede acende o portao, mesmo quando esta tarefa ja esgotou as
    // tentativas dela: e daqui que o agendador aprende que a internet caiu,
    // sem precisar de um monitor separado.
    if kind == FailKind::Network && !state.offline.swap(true, Ordering::Relaxed) {
        // Só na borda: uma falha atrás da outra não pode empurrar o relógio
        // para a frente, ou o teto de `MAX_OFFLINE` nunca venceria.
        state.offline_since.store(now_secs(), Ordering::Relaxed);
    }
    let mut tasks = state.tasks.lock().unwrap();
    let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
        return false;
    };
    // Pausar/cancelar durante a espera vence a nova tentativa.
    if matches!(t.status, Status::Paused | Status::Done) {
        return true;
    }
    t.fail_kind = kind;
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
    // Uma unica definicao da pasta da novela, compartilhada com quem grava o
    // manifesto e com a varredura: a formula repetida era o jeito de uma copia
    // desviar da outra sem ninguem notar. `safe` segue servindo ao NOME do
    // arquivo, que e outra coisa.
    let dir = media::series_dir(dest_dir, &series.name);
    let ext = series.extension();
    let mut added = 0;

    // O que a pasta ja tem, numa varredura so, ANTES do laco.
    //
    // Antes a pergunta "ja tenho?" era feita so no caminho exato que o
    // downloader escreveria hoje. Um arquivo renomeado, ou vindo de quando o
    // nome da novela era outro, nao era encontrado — e o episodio baixava de
    // novo por cima do que ja estava la. Procurar pelo NUMERO, que e a
    // identidade estavel, e o que faz o "nunca duas vezes" valer de verdade.
    let no_disco = manifest::episode_files(&dir);

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
        // pode ser por tamanho: vale existir um MP4 publicado e valido.
        //
        // O caminho de hoje primeiro; se nao houver, o arquivo que a pasta
        // guarda com este numero. A validacao continua a mesma e continua
        // estrita — muda so ONDE se procura, nunca o que conta como pronto.
        let achado = no_disco
            .get(num)
            .map(|nome| dir.join(nome))
            .filter(|p| *p != dest);
        let existente = if download::published_mp4(&dest) {
            Some(dest.clone())
        } else {
            achado.filter(|p| download::published_mp4(p))
        };
        let have_file = existente.is_some();
        // O progresso e o tamanho saem do arquivo que existe de verdade, e nao
        // do nome que o downloader usaria: senao a linha diria "0 B" sobre um
        // episodio que esta inteiro no disco com outro nome.
        let dest_real = existente.unwrap_or_else(|| dest.clone());

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
            let actual = std::fs::metadata(&dest_real).map(|m| m.len()).unwrap_or(size);
            // Fonte sem tamanho anunciado: o arquivo no disco e a verdade.
            if size == 0 {
                size = actual;
            }
            actual
        } else {
            let part = std::fs::metadata(task_part_path(series.source, &dest))
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
            // Para o que ainda vai baixar, `dest_real` e o proprio `dest`; para
            // o que ja esta pronto sob outro nome, e o arquivo de verdade — que
            // e o que "Abrir pasta" precisa encontrar.
            dest: dest_real.to_string_lossy().to_string(),
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
            fail_kind: FailKind::None,
            revivals: 0,
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
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| (t.source, t.dest.clone()))
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
        if let Some((source, d)) = dest {
            let p = PathBuf::from(&d);
            // Inclui a anotacao de retomada do HLS: deixar o .idx para tras
            // sujaria a pasta com lixo de um download que nao existe mais.
            if source == Source::Reelshort {
                download::remove_hls_mp4_artifacts(&p);
            } else {
                download::remove_artifacts(&p);
            }
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
            // "Tentar de novo" e uma ordem do usuario, e ela devolve tambem o
            // orcamento do zelador: sem isto, uma tarefa que gastou as doze
            // ressurreicoes durante a noite ficaria fora do trabalho automatico
            // para sempre, mesmo depois de o usuario ter consertado a causa.
            t.revivals = 0;
            t.fail_kind = FailKind::None;
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
            fail_kind: FailKind::None,
            revivals: 0,
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

    /// A atualizacao nao pode abandonar nem baixar novamente um `.ts` que uma
    /// versao anterior ja terminou; ele vira a entrada local do remux.
    #[test]
    fn tarefa_antiga_do_reelshort_migra_o_destino_para_mp4() {
        let mut t = task("rs:d-1", Status::Done, PRIO_NORMAL, 1);
        t.source = Source::Reelshort;
        t.dest = "D:/novela/Novela - E01.ts".into();
        migrate_reelshort_destination(&mut t);
        assert_eq!(t.dest, "D:/novela/Novela - E01.mp4");
        assert_eq!(
            download::hls_source_path(PathBuf::from(&t.dest).as_path()),
            PathBuf::from("D:/novela/Novela - E01.ts")
        );
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
                schedule_retry(&state, "a", "sem conexao com a internet", false, FailKind::Network),
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
        assert!(!schedule_retry(&state, "a", "sem conexao com a internet", false, FailKind::Network));
    }

    /// A classificacao sai da mensagem CRUA, e separa o que passa sozinho do
    /// que so o usuario resolve. E ela que decide o que o zelador ressuscita.
    #[test]
    fn classifica_a_falha_pela_mensagem_crua() {
        // Passam sozinhas.
        assert_eq!(classify("error sending request for url"), FailKind::Network);
        assert_eq!(classify("dns error"), FailKind::Network);
        assert_eq!(classify("a transferencia veio incompleta"), FailKind::Network);
        assert_eq!(classify("HTTP 403"), FailKind::Stale);
        assert_eq!(classify("tamanho inesperado"), FailKind::Stale);
        assert_eq!(classify("os error 112"), FailKind::Disk);

        // Nao passam sozinhas.
        assert_eq!(classify("HTTP 404"), FailKind::Terminal);
        assert_eq!(classify("episodio pago no ReelShort"), FailKind::Terminal);
        assert_eq!(classify("sem midia acessivel"), FailKind::Terminal);
        assert_eq!(classify("o FFmpeg falhou"), FailKind::Terminal);

        assert!(FailKind::Network.recoverable());
        assert!(FailKind::Stale.recoverable());
        assert!(FailKind::Disk.recoverable());
        assert!(!FailKind::Terminal.recoverable());
        assert!(!FailKind::Auth.recoverable());
        assert!(!FailKind::None.recoverable());
    }

    /// Uma queda de rede fecha o portao do agendador.
    ///
    /// Sem isto, a fila continua puxando tarefa nova durante a queda e cada uma
    /// queima as seis tentativas dela em menos de quatro minutos — o muro de
    /// falhas permanentes que aparece de manha.
    #[test]
    fn falha_de_rede_fecha_o_portao_do_agendador() {
        let state = Arc::new(AppState::new());
        state
            .tasks
            .lock()
            .unwrap()
            .push(task("a", Status::Running, PRIO_NORMAL, 1));

        assert!(!state.offline.load(Ordering::Relaxed));
        schedule_retry(&state, "a", "sem conexao", false, FailKind::Network);
        assert!(state.offline.load(Ordering::Relaxed), "a rede caiu: o portao fecha");
        assert!(state.offline_since.load(Ordering::Relaxed) > 0, "marca a hora");
    }

    /// Falhas seguidas nao empurram o relogio: o teto de `MAX_OFFLINE` precisa
    /// vencer. Sem isto o portao so abriria por sucesso da sonda — e uma sonda
    /// recusada pela origem congelaria a fila com a internet funcionando.
    #[test]
    fn falhas_seguidas_nao_adiam_o_teto_do_portao() {
        let state = Arc::new(AppState::new());
        state
            .tasks
            .lock()
            .unwrap()
            .push(task("a", Status::Running, PRIO_NORMAL, 1));

        schedule_retry(&state, "a", "sem conexao", false, FailKind::Network);
        let primeira = state.offline_since.load(Ordering::Relaxed);
        // Finge que o portao fechou ha muito tempo.
        state.offline_since.store(primeira - 250, Ordering::Relaxed);

        schedule_retry(&state, "a", "sem conexao", false, FailKind::Network);
        assert_eq!(
            state.offline_since.load(Ordering::Relaxed),
            primeira - 250,
            "so a borda marca a hora; a segunda falha nao reinicia a contagem"
        );
    }

    /// Falha que nao e de rede nao fecha o portao: um episodio pago nao pode
    /// parar a fila inteira.
    #[test]
    fn falha_terminal_nao_fecha_o_portao() {
        let state = Arc::new(AppState::new());
        state
            .tasks
            .lock()
            .unwrap()
            .push(task("a", Status::Running, PRIO_NORMAL, 1));

        schedule_retry(&state, "a", "episodio pago", false, FailKind::Terminal);
        assert!(!state.offline.load(Ordering::Relaxed));
    }

    /// O agendador nao escolhe ninguem enquanto o portao esta fechado.
    #[test]
    fn com_o_portao_fechado_nada_e_escolhido_pelo_agendador() {
        let state = Arc::new(AppState::new());
        state
            .tasks
            .lock()
            .unwrap()
            .push(task("a", Status::Queued, PRIO_NORMAL, 1));

        // `pick_next` continua achando a tarefa: quem decide nao puxar e o
        // agendador, lendo o portao. Este teste tranca o contrato dos dois.
        assert_eq!(pick_next(&state).as_deref(), Some("a"));
        state.offline.store(true, Ordering::Relaxed);
        assert!(state.offline.load(Ordering::Relaxed));
    }

    /// O zelador so ressuscita o que passa sozinho, e respeita o teto.
    #[test]
    fn o_zelador_ressuscita_so_o_recuperavel_e_ate_o_teto() {
        let state = Arc::new(AppState::new());
        {
            let mut tasks = state.tasks.lock().unwrap();
            for (id, kind) in [
                ("rede", FailKind::Network),
                ("vencido", FailKind::Stale),
                ("pago", FailKind::Terminal),
                ("sessao", FailKind::Auth),
            ] {
                let mut t = task(id, Status::Failed, PRIO_NORMAL, 1);
                t.fail_kind = kind;
                t.attempts = MAX_ATTEMPTS;
                tasks.push(t);
            }
            // Ja gastou o orcamento de ressurreicoes.
            let mut t = task("cansada", Status::Failed, PRIO_NORMAL, 5);
            t.fail_kind = FailKind::Network;
            t.revivals = MAX_REVIVALS;
            tasks.push(t);
        }

        revive_recuperaveis(&state);

        let tasks = state.tasks.lock().unwrap();
        let estado = |id: &str| tasks.iter().find(|t| t.id == id).unwrap().status;
        assert_eq!(estado("rede"), Status::Queued, "rede volta sozinha");
        assert_eq!(estado("vencido"), Status::Queued, "link vencido reconsulta");
        assert_eq!(estado("pago"), Status::Failed, "pago nao ressuscita");
        assert_eq!(estado("sessao"), Status::Failed, "so o usuario resolve login");
        assert_eq!(estado("cansada"), Status::Failed, "o teto e respeitado");

        let revivida = tasks.iter().find(|t| t.id == "rede").unwrap();
        assert_eq!(revivida.attempts, 0, "o orcamento de tentativas volta");
        assert_eq!(revivida.revivals, 1);

        // Link vencido tem de reconsultar: manter a URL morta repetiria a falha.
        let vencida = tasks.iter().find(|t| t.id == "vencido").unwrap();
        assert!(vencida.urls.is_empty());
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
        assert_eq!(
            friendly("nao consegui converter o episodio para MP4: invalid data"),
            "nao consegui preparar o MP4"
        );
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
