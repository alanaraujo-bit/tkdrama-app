import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./styles.css";

// ------------------------------------------------------------------- tipos

type Status = "queued" | "running" | "paused" | "done" | "failed" | "blocked";

interface Gear {
  gear_name: string;
  codec: string;
  width: number;
  height: number;
  bitrate: number;
  size: number;
  urls: string[];
}

type SourceName = "tiktok" | "reelshort" | "candyjar";

interface Episode {
  number: number;
  item_id: string;
  duration: number;
  gears: Gear[];
  /** Existe, mas e pago/trancado na plataforma. Nunca vira download. */
  locked: boolean;
  lock_reason: string;
}

interface Series {
  drama_id: string;
  name: string;
  description: string;
  /**
   * Espelhos da capa, assinados e temporários.
   *
   * Servem para a prévia aqui na tela e para o app baixar o `poster.jpg` da
   * pasta. Nunca são gravados: expiram em poucos dias.
   */
  covers: string[];
  total: number;
  episodes: Episode[];
  source: SourceName;
  lang: string;
}

/** Espelha `Source::tag()` em media.rs — o id da tarefa leva a fonte no prefixo. */
const SOURCE_TAG: Record<string, string> = { tiktok: "tk", reelshort: "rs", candyjar: "cj" };

/** Espelha `Source::label()` em media.rs — usado so para textos da tela. */
const SOURCE_LABEL: Record<string, string> = {
  tiktok: "TikTok",
  reelshort: "ReelShort",
  candyjar: "CandyJar",
};

function taskId(source: string, dramaId: string, episode: number): string {
  return `${SOURCE_TAG[source] ?? "tk"}:${dramaId}-${episode}`;
}

/** Da para baixar? Espelha `Episode::available()` no Rust. */
function available(ep: Episode): boolean {
  return !ep.locked && ep.gears.length > 0;
}

interface Task {
  id: string;
  drama_id: string;
  series: string;
  episode: number;
  dest: string;
  gear_name: string;
  codec: string;
  width: number;
  height: number;
  duration: number;
  size: number;
  downloaded: number;
  status: Status;
  priority: number;
  error: string | null;
  seq: number;
  source: SourceName;
  speed: number;
  attempts: number;
  retry_at: number;
  elapsed_secs: number;
  created_at: number;
  finished_at: number | null;
  active_since?: number;
}

/** Espelha `MAX_ATTEMPTS` em queue.rs — usado so para escrever "(3/6)" na tela. */
const MAX_ATTEMPTS = 6;

/** Numero confiavel ou 0: o backend pode omitir um campo, e um NaN escapando
 *  daqui vira "NaNhNaN" na tela em vez de um erro visivel. */
function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

interface Settings {
  dest_dir: string;
  concurrency: number;
  codec: string;
  gear: string | null;
  sequential: boolean;
  auto_enabled: boolean;
  auto_interval_hours: number;
  auto_max_new_series: number;
  auto_max_gb_per_run: number;
  auto_min_free_gb: number;
}

/** Uma novela que o app revisita sozinho. Espelha `watch::Follow` no Rust. */
interface Follow {
  source: SourceName;
  drama_id: string;
  name: string;
  url: string;
  added_at: number;
  last_check: number;
  last_added: number;
  paused: boolean;
  error: string | null;
}

/** Espelha `watch::Follow::key()` — a chave que os comandos usam. */
function followKey(f: Follow): string {
  return `${SOURCE_TAG[f.source] ?? "tk"}:${f.drama_id}`;
}

/** Resultado de uma rodada. Espelha `watch::Report`. */
interface WatchReport {
  checked: number;
  added: number;
  bytes: number;
  errors: string[];
  stopped: string | null;
}

// ------------------------------------------------------------------ estado

let series: Series | null = null;
/** URL que trouxe a serie atual. O acompanhamento guarda a URL, nao o id: o
 *  ReelShort precisa do idioma da pagina para remontar os episodios depois. */
let seriesUrl = "";
let tasks: Task[] = [];
let follows: Follow[] = [];
let settings: Settings = {
  dest_dir: "",
  concurrency: 3,
  codec: "h264",
  gear: null,
  sequential: false,
  auto_enabled: false,
  auto_interval_hours: 6,
  auto_max_new_series: 10,
  auto_max_gb_per_run: 5,
  auto_min_free_gb: 10,
};
let selected = new Set<number>();
let freeBytes: number | null = null;

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

// ---------------------------------------------------------------- formatos

function bytes(n: number): string {
  const total = num(n);
  if (!total) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = total;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${u[i]}`;
}

function speed(n: number): string {
  return num(n) > 0 ? `${bytes(n)}/s` : "";
}

function eta(task: Task): string {
  const rate = num(task.speed);
  // `> 0` e nao `<= 0`: com um valor ausente a comparacao invertida passava
  // batido e o calculo seguinte imprimia "NaNhNaN" na linha.
  if (task.status !== "running" || rate <= 0) return "";
  const left = num(task.size) - num(task.downloaded);
  if (left <= 0) return "";
  return duration(left / rate);
}

function duration(seconds: number): string {
  const s = Math.max(0, Math.round(num(seconds)));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}min`;
  const h = Math.floor(m / 60);
  return `${h}h${String(m % 60).padStart(2, "0")}min`;
}

// Marca + cor: o estado precisa ser legivel sem enxergar cor.
const MARK: Record<Status, string> = {
  queued: "○",
  running: "●",
  paused: "‖",
  done: "✓",
  failed: "✕",
  blocked: "⊘",
};

const LABEL: Record<Status, string> = {
  queued: "na fila",
  running: "baixando",
  paused: "pausado",
  done: "concluído",
  failed: "falhou",
  blocked: "bloqueado",
};

/** "há 3h", "agora mesmo". Recebe segundos epoch (o relogio do backend). */
function ago(epochSecs: number): string {
  const d = Math.max(0, Math.floor(Date.now() / 1000) - num(epochSecs));
  if (d < 60) return "agora mesmo";
  if (d < 3600) return `há ${Math.floor(d / 60)}min`;
  if (d < 86400) return `há ${Math.floor(d / 3600)}h`;
  return `há ${Math.floor(d / 86400)}d`;
}

function banner(msg: string, kind: "" | "is-error" | "is-ok" = "") {
  const el = $("#banner");
  el.textContent = msg;
  el.className = `banner ${kind}`.trim();
  el.hidden = !msg;
  if (msg && kind === "is-ok") {
    setTimeout(() => {
      if (el.textContent === msg) el.hidden = true;
    }, 4000);
  }
}

// -------------------------------------------------------------- autenticacao

interface AuthEntry {
  logged: boolean;
  cookies: number;
  /** Da para AFIRMAR que esta logado, ou so da para dizer que ha cookies? */
  certain: boolean;
  label: string;
}

async function refreshAuth() {
  const chips: Array<[string, string, string]> = [
    ["tk", "#auth-chip", "#auth-label"],
    ["rs", "#auth-chip-rs", "#auth-label-rs"],
    ["cj", "#auth-chip-cj", "#auth-label-cj"],
  ];
  try {
    const r = await invoke<Record<string, AuthEntry>>("auth_status");
    for (const [tag, chipSel, labelSel] of chips) {
      const entry = r[tag];
      const chip = $(chipSel);
      const label = $(labelSel);
      if (!entry) {
        chip.hidden = true;
        continue;
      }
      chip.hidden = false;
      if (entry.logged) {
        chip.className = "chip is-on";
        // Sem cookie de sessao conhecido nao da para prometer "conectado":
        // dizer isso sem poder provar seria a interface mentindo de novo.
        label.textContent = entry.certain
          ? `${entry.label}: conectado`
          : `${entry.label}: sessão detectada`;
        chip.title = entry.certain
          ? `Sessão do ${entry.label} ativa. Clique para reconectar.`
          : `${entry.cookies} cookies do ${entry.label} guardados. Não dá para confirmar o login daqui — use "O que minha conta libera" para ver o que ela enxerga.`;
      } else {
        chip.className = "chip is-off";
        label.textContent = `entrar no ${entry.label}`;
        chip.title = `Entrar no ${entry.label} para o app enxergar o que a sua conta libera.`;
      }
    }
  } catch (e) {
    // Nunca deixar o rotulo inicial preso: isso mente sobre o estado.
    for (const [, chipSel, labelSel] of chips) {
      const chip = $(chipSel);
      chip.className = "chip is-off";
      chip.hidden = false;
      $(labelSel).textContent = "entrar";
      chip.title = `Nao consegui ler a sessao (${e}). Clique para entrar.`;
    }
  }
}

// ---------------------------------------------------------------- ajustes

async function loadSettings() {
  settings = await invoke<Settings>("settings_get");
  $<HTMLInputElement>("#set-dest").value = settings.dest_dir;
  const conc = $<HTMLInputElement>("#set-conc");
  conc.value = String(settings.concurrency);
  $("#set-conc-out").textContent = String(settings.concurrency);
  const radio = document.querySelector<HTMLInputElement>(
    `input[name="codec"][value="${settings.codec}"]`,
  );
  if (radio) radio.checked = true;
  $<HTMLInputElement>("#set-sequential").checked = settings.sequential;
  $<HTMLInputElement>("#set-auto").checked = settings.auto_enabled;
  const iv = $<HTMLInputElement>("#set-auto-interval");
  iv.value = String(settings.auto_interval_hours);
  $("#set-auto-interval-out").textContent = `${settings.auto_interval_hours}h`;
  $<HTMLInputElement>("#set-auto-series").value = String(settings.auto_max_new_series);
  $<HTMLInputElement>("#set-auto-gb").value = String(settings.auto_max_gb_per_run);
  $<HTMLInputElement>("#set-auto-free").value = String(settings.auto_min_free_gb);
}

async function saveSettings() {
  const codec =
    document.querySelector<HTMLInputElement>('input[name="codec"]:checked')?.value ?? "h264";
  const next: Settings = {
    dest_dir: $<HTMLInputElement>("#set-dest").value.trim(),
    concurrency: Number($<HTMLInputElement>("#set-conc").value),
    codec,
    gear: null,
    sequential: $<HTMLInputElement>("#set-sequential").checked,
    auto_enabled: $<HTMLInputElement>("#set-auto").checked,
    auto_interval_hours: Number($<HTMLInputElement>("#set-auto-interval").value),
    // `num` para nao mandar NaN de um campo vazio: no Rust isso viraria erro de
    // desserializacao e os ajustes inteiros deixariam de salvar.
    auto_max_new_series: num(Number($<HTMLInputElement>("#set-auto-series").value)),
    auto_max_gb_per_run: num(Number($<HTMLInputElement>("#set-auto-gb").value)),
    auto_min_free_gb: num(Number($<HTMLInputElement>("#set-auto-free").value)),
  };
  try {
    await invoke("settings_set", { settings: next });
    settings = next;
    $("#settings-modal").hidden = true;
    banner("Ajustes salvos.", "is-ok");
    // O cabecalho do painel anuncia o intervalo: sem isto continuaria dizendo
    // o valor antigo ate a proxima recarga.
    renderFollows();
    refreshDisk();
  } catch (e) {
    banner(String(e), "is-error");
  }
}

// ------------------------------------------------------------------ serie

async function fetchSeries() {
  const url = $<HTMLInputElement>("#url-input").value.trim();
  if (!url) {
    banner("Cole o link de uma novela do TikTok, do ReelShort ou do CandyJar primeiro.", "is-error");
    return;
  }
  const btn = $<HTMLButtonElement>("#fetch-btn");
  btn.disabled = true;
  btn.textContent = "Buscando...";
  banner("");

  try {
    series = await invoke<Series>("fetch_series", { url });
    seriesUrl = url;
    selected = new Set();
    renderSeries();
    // O portão de direito do ReelShort e do CandyJar depende da sessão da
    // janela logada. Se sobrou episódio trancado, o motivo mais provável é
    // ela não ter sido capturada — dizer isso é melhor do que deixar o
    // usuário no escuro.
    if (series.source !== "tiktok" && series.episodes.some((e) => e.locked)) {
      const n = series.episodes.filter((e) => e.locked).length;
      const label = SOURCE_LABEL[series.source] ?? series.source;
      banner(
        `${n} episódio(s) ainda trancados. Se a sua conta libera eles, mantenha a janela do ${label} aberta e logada (chip lá em cima) e clique em Buscar de novo.`,
      );
    }
  } catch (e) {
    banner(String(e), "is-error");
    series = null;
    $("#series-panel").hidden = true;
  } finally {
    btn.disabled = false;
    btn.textContent = "Buscar";
  }
}

function episodeSize(ep: Episode): number {
  const pool = ep.gears.filter((g) => g.codec.startsWith(settings.codec));
  const list = pool.length ? pool : ep.gears;
  return list.reduce((best, g) => (g.bitrate > best.bitrate ? g : best), list[0])?.size ?? 0;
}

function renderSeries() {
  if (!series) return;
  const panel = $("#series-panel");
  panel.hidden = false;

  const cover = $<HTMLImageElement>("#series-cover");
  if (series.covers.length) {
    cover.src = series.covers[0];
    cover.hidden = false;
  } else {
    cover.hidden = true;
  }

  $("#series-name").textContent = series.name;
  $("#series-desc").textContent = series.description;

  const total = series.episodes.reduce((s, ep) => s + episodeSize(ep), 0);
  const locked = series.episodes.filter((ep) => ep.locked).length;
  const blocked = series.episodes.filter((ep) => !ep.locked && ep.gears.length === 0).length;
  // O ReelShort nao anuncia tamanho: melhor omitir do que mostrar "—" como se
  // fosse zero. O total aparece conforme os episodios baixam.
  const parts = [`${series.episodes.length} episódios`];
  if (total > 0) parts.push(bytes(total));
  if (locked) parts.push(`${locked} pago${locked > 1 ? "s" : ""}`);
  if (blocked) parts.push(`${blocked} bloqueado${blocked > 1 ? "s" : ""}`);
  $("#series-stats").textContent = parts.join(" · ");

  renderPicker();
  updatePickSummary();
}

function haveIds(): Set<string> {
  return new Set(tasks.filter((t) => t.status === "done").map((t) => t.id));
}

function episodeKey(ep: Episode): string {
  return taskId(series!.source, series!.drama_id, ep.number);
}

function renderPicker() {
  if (!series) return;
  const grid = $("#ep-grid");
  const have = haveIds();
  grid.innerHTML = "";

  for (const ep of series.episodes) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "ep";
    b.textContent = String(ep.number);
    b.dataset.ep = String(ep.number);

    if (!available(ep)) {
      b.classList.add("is-blocked");
      b.disabled = true;
      b.title = ep.locked
        ? `Episódio ${ep.number}: ${ep.lock_reason || "pago"} — desbloqueie na plataforma`
        : `Episódio ${ep.number}: sem mídia acessível`;
    } else {
      if (have.has(episodeKey(ep))) b.classList.add("is-have");
      if (selected.has(ep.number)) b.classList.add("is-sel");
      const size = episodeSize(ep);
      const detalhes = [`Episódio ${ep.number}`];
      if (size > 0) detalhes.push(bytes(size));
      if (ep.duration > 0) detalhes.push(`${ep.duration}s`);
      b.title = detalhes.join(" · ");
    }
    grid.appendChild(b);
  }
}

function updatePickSummary() {
  if (!series) return;
  const n = selected.size;
  const size = series.episodes
    .filter((ep) => selected.has(ep.number))
    .reduce((s, ep) => s + episodeSize(ep), 0);
  $("#pick-summary").textContent = n ? `${n} selecionado${n > 1 ? "s" : ""} · ${bytes(size)}` : "";
  $<HTMLButtonElement>("#download-selected").disabled = n === 0;
}

function setSelection(next: Set<number>) {
  selected = next;
  for (const el of document.querySelectorAll<HTMLElement>(".ep")) {
    const n = Number(el.dataset.ep);
    el.classList.toggle("is-sel", selected.has(n));
  }
  updatePickSummary();
}

// --------------------------------------------------------- acompanhamento

async function refreshFollows() {
  follows = await invoke<Follow[]>("follow_list");
  renderFollows();
}

function renderFollows() {
  const panel = $("#watch-panel");
  panel.hidden = follows.length === 0;
  $("#watch-next").textContent = settings.auto_enabled
    ? `verificando a cada ${settings.auto_interval_hours}h`
    : "verificação automática desligada";

  const list = $("#watch-list");
  list.textContent = "";
  for (const f of follows) {
    const row = document.createElement("div");
    row.className = "watch-row" + (f.paused ? " is-paused" : "");
    row.setAttribute("role", "listitem");

    const info = document.createElement("div");
    info.className = "watch-info";
    const nome = document.createElement("strong");
    nome.textContent = f.name;
    const sub = document.createElement("span");
    sub.className = "muted";
    // Nunca "verificado há 56 anos": last_check 0 quer dizer que nunca rodou.
    const quando = f.last_check ? `verificado ${ago(f.last_check)}` : "ainda não verificado";
    const novos = f.last_added > 0 ? ` · ${f.last_added} novo(s) na última` : "";
    sub.textContent = f.error
      ? `${f.error}`
      : `${SOURCE_LABEL[f.source] ?? "TikTok"} · ${quando}${novos}`;
    if (f.error) sub.classList.add("is-error-text");
    info.append(nome, sub);

    const tools = document.createElement("div");
    tools.className = "watch-tools";

    const pausar = document.createElement("button");
    pausar.className = "btn btn-sm";
    pausar.type = "button";
    pausar.textContent = f.paused ? "Retomar" : "Pausar";
    pausar.addEventListener("click", async () => {
      await invoke("follow_set_paused", { key: followKey(f), paused: !f.paused });
      refreshFollows();
    });

    const parar = document.createElement("button");
    parar.className = "btn btn-sm";
    parar.type = "button";
    parar.textContent = "Deixar de acompanhar";
    parar.addEventListener("click", async () => {
      await invoke("follow_remove", { key: followKey(f) });
      banner(`Não acompanho mais "${f.name}". O que já baixou continua na pasta.`, "is-ok");
      refreshFollows();
    });

    tools.append(pausar, parar);
    row.append(info, tools);
    list.append(row);
  }
}

async function followCurrent() {
  if (!series) return;
  try {
    const novo = await invoke<boolean>("follow_add", {
      dramaId: series.drama_id,
      url: seriesUrl,
      source: series.source,
    });
    banner(
      novo
        ? `Acompanhando "${series.name}". Episódio novo entra na fila sozinho.`
        : `"${series.name}" já estava sendo acompanhada.`,
      "is-ok",
    );
    refreshFollows();
  } catch (e) {
    banner(String(e), "is-error");
  }
}

async function runWatchNow() {
  const btn = $<HTMLButtonElement>("#watch-now");
  btn.disabled = true;
  btn.textContent = "Verificando...";
  try {
    const r = await invoke<WatchReport>("watch_run_now");
    const partes: string[] = [];
    partes.push(
      r.added > 0
        ? `${r.added} episódio(s) novo(s) na fila`
        : `nada novo em ${r.checked} novela(s)`,
    );
    if (r.stopped) partes.push(r.stopped);
    if (r.errors.length) partes.push(r.errors.join(" | "));
    banner(partes.join(" · "), r.errors.length ? "is-error" : "is-ok");
    await refreshFollows();
    refreshQueue();
  } catch (e) {
    banner(String(e), "is-error");
  } finally {
    btn.disabled = false;
    btn.textContent = "Verificar agora";
  }
}

// -------------------------------------------------------------- metadados

interface MetaReport {
  folders: number;
  updated: number;
  posters: number;
  thumbs: number;
  episodes: number;
  converted: number;
  unresolved: string[];
  errors: string[];
}

/**
 * As pastas que a varredura não conseguiu identificar.
 *
 * Ficam listadas por nome porque é assim que o usuário as encontra no TikTok
 * para colar o link. Adivinhar qual novela é cada uma seria pior que não
 * preencher: gravaria a ficha de uma novela na pasta de outra.
 */
function renderUnresolved(nomes: string[]) {
  const box = $("#meta-unresolved");
  const list = $("#meta-unresolved-list");
  list.textContent = "";
  for (const nome of nomes) {
    const li = document.createElement("li");
    li.textContent = nome;
    list.appendChild(li);
  }
  box.hidden = nomes.length === 0;
}

/**
 * Grava a ficha das novelas que já estão no disco.
 *
 * A varredura é lenta de propósito — 20s entre uma novela e outra, pelo mesmo
 * motivo do acompanhamento automático — então o botão mostra em qual pasta
 * está, senão a janela parece travada.
 */
async function runMetadata() {
  const btn = $<HTMLButtonElement>("#meta-refresh");
  const status = $("#meta-status");
  btn.disabled = true;
  btn.textContent = "Atualizando...";
  status.textContent = "";
  try {
    const r = await invoke<MetaReport>("metadata_refresh");
    const partes = [
      `${r.updated} de ${r.folders} novela(s)`,
      `${r.posters} capa(s)`,
      `${r.thumbs} miniatura(s)`,
    ];
    if (r.converted) partes.push(`${r.converted} TS convertido(s)`);
    // Pasta sem identidade não é erro nem palpite: é trabalho que sobrou para o
    // usuário. O banner conta quantas; a lista embaixo diz QUAIS, porque é ela
    // que ele vai usar para buscar uma por uma.
    if (r.unresolved.length) partes.push(`${r.unresolved.length} sem identificar`);
    if (r.errors.length) partes.push(r.errors.join(" | "));
    banner(partes.join(" · "), r.errors.length ? "is-error" : "is-ok");
    status.textContent = `${r.episodes} episódio(s) com ficha`;
    renderUnresolved(r.unresolved);
  } catch (e) {
    banner(String(e), "is-error");
  } finally {
    btn.disabled = false;
    btn.textContent = "Atualizar metadados";
  }
}

// ------------------------------------------------------------------- fila

async function enqueue(episodes: number[], priority = 1, sequential?: boolean) {
  if (!series || episodes.length === 0) return;

  const size = series.episodes
    .filter((ep) => episodes.includes(ep.number))
    .reduce((s, ep) => s + episodeSize(ep), 0);
  if (freeBytes !== null && size > freeBytes) {
    banner(
      `Sem espaço: precisa de ${bytes(size)} e há ${bytes(freeBytes)} livres em ${settings.dest_dir}.`,
      "is-error",
    );
    return;
  }

  try {
    const added = await invoke<number>("enqueue", {
      dramaId: series.drama_id,
      episodes,
      priority,
      sequential,
      // Sem a fonte o backend procuraria a serie na chave errada do cache.
      source: series.source,
    });
    banner(
      added === 0
        ? "Esses episódios já estavam na fila."
        : `${added} episódio${added > 1 ? "s" : ""} na fila${sequential ? ", em sequência" : ""}.`,
      "is-ok",
    );
    refreshQueue();
  } catch (e) {
    banner(String(e), "is-error");
  }
}

async function refreshQueue() {
  tasks = await invoke<Task[]>("queue_list");
  renderQueue();
}

const ICON_PAUSE = `<svg viewBox="0 0 24 24"><path d="M9 5v14M15 5v14"/></svg>`;
const ICON_PLAY = `<svg viewBox="0 0 24 24"><path d="M7 4l13 8-13 8z"/></svg>`;
const ICON_FOLDER = `<svg viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>`;
const ICON_X = `<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>`;

// Reconciliacao por chave: as linhas sao atualizadas no lugar, sem recriar o
// DOM, para 50 itens atualizando 4x/s nao piscarem nem perderem o foco.
//
// As referencias aos filhos sao guardadas na criacao. Antes cada quadro
// reconsultava o DOM por seletor de atributo e o proprio render trocava esse
// atributo ao concluir um episodio: da vez seguinte o seletor nao achava mais
// nada, o render estourava e a lista inteira congelava para sempre. Com as
// referencias fixas nao existe seletor para quebrar.
interface Row {
  el: HTMLElement;
  mark: HTMLElement;
  ep: HTMLElement;
  name: HTMLElement;
  sub: HTMLElement;
  fill: HTMLElement;
  size: HTMLElement;
  rate: HTMLElement;
  toggle: HTMLButtonElement;
  open: HTMLButtonElement;
  cancel: HTMLButtonElement;
  mode: string;
}

const rows = new Map<string, Row>();

function buildRow(id: string): Row {
  const el = document.createElement("div");
  el.className = "row";
  el.dataset.id = id;
  el.setAttribute("role", "listitem");
  el.innerHTML = `
    <span class="row-mark"></span>
    <span class="row-ep"></span>
    <span class="row-title"><span class="row-name"></span><span class="row-sub"></span></span>
    <span class="bar"><span class="bar-fill"></span></span>
    <span class="row-size"></span>
    <span class="row-speed"></span>
    <span class="row-actions">
      <button class="icon-btn" data-act="prio" title="Prioridade" aria-label="Prioridade">
        <svg viewBox="0 0 24 24"><path d="M12 19V5M5 12l7-7 7 7"/></svg>
      </button>
      <button class="icon-btn" data-act="toggle" aria-label="Pausar">${ICON_PAUSE}</button>
      <button class="icon-btn" data-act="open" title="Abrir pasta" aria-label="Abrir pasta" hidden>${ICON_FOLDER}</button>
      <button class="icon-btn" data-act="cancel" title="Remover da fila" aria-label="Remover da fila">${ICON_X}</button>
    </span>`;

  const pick = <T extends HTMLElement>(sel: string) => el.querySelector<T>(sel)!;
  return {
    el,
    mark: pick(".row-mark"),
    ep: pick(".row-ep"),
    name: pick(".row-name"),
    sub: pick(".row-sub"),
    fill: pick(".bar-fill"),
    size: pick(".row-size"),
    rate: pick(".row-speed"),
    toggle: pick<HTMLButtonElement>('[data-act="toggle"]'),
    open: pick<HTMLButtonElement>('[data-act="open"]'),
    cancel: pick<HTMLButtonElement>('[data-act="cancel"]'),
    mode: "",
  };
}

/** Escreve so quando muda: evita trocar 400 nos de texto quatro vezes por segundo. */
function setText(el: HTMLElement, value: string) {
  if (el.textContent !== value) el.textContent = value;
}

/** Texto da segunda linha: o que a tarefa esta fazendo, em portugues claro. */
function subtitle(t: Task): string {
  if (t.status === "running") {
    // Resolucao so entra quando e conhecida: o ReelShort nao anuncia, e "0x0"
    // na linha e pior do que campo nenhum.
    const res = t.width > 0 && t.height > 0 ? `${t.width}x${t.height}` : "";
    return [t.gear_name, res, eta(t)].filter(Boolean).join(" · ");
  }
  // Uma tarefa que ja cumpriu a espera mas ainda nao foi pega pelo agendador
  // nao pode continuar mostrando uma contagem regressiva vencida: e nessa
  // fresta que a tela deixa de bater com o que esta realmente acontecendo.
  if (t.status === "queued" && num(t.attempts) > 0) {
    const wait = Math.ceil(num(t.retry_at) - Date.now() / 1000);
    const reason = (t.error ?? "").split(" — ")[0] || "falha de rede";
    const tentativa = `${num(t.attempts) + 1}/${MAX_ATTEMPTS}`;
    return wait > 0
      ? `${reason} — nova tentativa em ${wait}s (${tentativa})`
      : `${reason} — tentando de novo (${tentativa})`;
  }
  if (t.error) return t.error;
  return LABEL[t.status];
}

/// Fronteira externa: nada que aconteca aqui dentro pode impedir o proximo
/// quadro. O congelamento da fila nasceu de uma excecao nao tratada no render,
/// que matava todas as atualizacoes seguintes sem deixar rastro na tela.
function renderQueue() {
  try {
    renderQueueInner();
  } catch (e) {
    console.error("render da fila", e);
    banner("Erro ao desenhar a fila. Os downloads continuam.", "is-error");
  }
  try {
    renderStatusBar();
  } catch (e) {
    console.error("render do rodape", e);
  }
}

function renderQueueInner() {
  const list = $("#queue-list");
  const empty = $("#queue-empty");
  empty.hidden = tasks.length > 0;

  // Ordem: rodando, depois na fila por prioridade, depois o resto.
  const rank: Record<Status, number> = {
    running: 0,
    queued: 1,
    paused: 2,
    failed: 3,
    blocked: 4,
    done: 5,
  };
  const sorted = [...tasks].sort(
    (a, b) =>
      rank[a.status] - rank[b.status] ||
      b.priority - a.priority ||
      a.seq - b.seq,
  );

  const seen = new Set<string>();
  let prev: HTMLElement | null = null;
  let broke = 0;

  for (const t of sorted) {
    seen.add(t.id);
    let row = rows.get(t.id);
    if (!row) {
      row = buildRow(t.id);
      rows.set(t.id, row);
    }

    // Cada linha e isolada: se uma unica falhar, as outras continuam vivas.
    // Foi exatamente esse o defeito que travava a fila inteira.
    try {
      paintRow(row, t);
    } catch (e) {
      broke++;
      console.error("linha", t.id, e);
    }

    // Reposiciona sem recriar.
    const next: Element | null = prev ? prev.nextElementSibling : list.firstElementChild;
    if (next !== row.el) list.insertBefore(row.el, next);
    prev = row.el;
  }

  for (const [id, row] of rows) {
    if (!seen.has(id)) {
      row.el.remove();
      rows.delete(id);
    }
  }

  if (broke) banner(`${broke} linha(s) nao puderam ser desenhadas.`, "is-error");
}

function paintRow(row: Row, t: Task) {
  const prioClass = t.priority === 2 ? "prio-high" : t.priority === 0 ? "prio-low" : "";
  const cls = `row is-${t.status} ${prioClass}`.trim();
  if (row.el.className !== cls) row.el.className = cls;

  setText(row.mark, MARK[t.status] ?? "○");
  row.mark.title = LABEL[t.status] ?? t.status;
  setText(row.ep, `E${String(t.episode).padStart(2, "0")}`);
  setText(row.name, t.series);
  setText(row.sub, subtitle(t));
  row.sub.classList.toggle("row-err", t.status === "failed");

  const size = num(t.size);
  const got = Math.min(num(t.downloaded), size || num(t.downloaded));
  // Concluido e 100% por definicao, mesmo que o tamanho anunciado divirja.
  const pct = t.status === "done" ? 100 : size > 0 ? Math.min(100, (got / size) * 100) : 0;
  const width = `${pct.toFixed(1)}%`;
  if (row.fill.style.width !== width) row.fill.style.width = width;

  setText(
    row.size,
    t.status === "done" ? bytes(size || got) : size ? `${bytes(got)} / ${bytes(size)}` : bytes(got),
  );
  setText(row.rate, speed(t.speed));

  row.toggle.hidden = t.status === "done" || t.status === "blocked";
  const willPause = t.status === "running" || t.status === "queued";
  const mode = willPause ? "pause" : "play";
  if (row.mode !== mode) {
    row.mode = mode;
    row.toggle.innerHTML = willPause ? ICON_PAUSE : ICON_PLAY;
    row.toggle.title = willPause ? "Pausar" : "Retomar";
    row.toggle.setAttribute("aria-label", row.toggle.title);
  }

  // Alternar visibilidade, nunca reescrever `data-act`: era a mutacao do
  // atributo que invalidava o seletor e derrubava o render.
  row.open.hidden = t.status !== "done";
  row.cancel.hidden = t.status === "done";
}

function renderStatusBar() {
  const running = tasks.filter((t) => t.status === "running");
  const queued = tasks.filter((t) => t.status === "queued").length;
  const done = tasks.filter((t) => t.status === "done").length;
  const failed = tasks.filter((t) => t.status === "failed").length;

  const retrying = tasks.filter(
    (t) => t.status === "queued" && num(t.retry_at) * 1000 > Date.now(),
  ).length;

  const parts: string[] = [];
  if (running.length) parts.push(`${running.length} baixando`);
  if (queued - retrying > 0) parts.push(`${queued - retrying} na fila`);
  if (retrying) parts.push(`${retrying} tentando de novo`);
  if (done) parts.push(`${done} pronto${done > 1 ? "s" : ""}`);
  if (failed) parts.push(`${failed} com falha`);
  $("#stat-counts").textContent = parts.join(" · ") || "ocioso";

  const total = running.reduce((s, t) => s + num(t.speed), 0);
  $("#stat-speed").textContent = speed(total);

  const trackable = tasks.filter((t) => ["queued", "running", "paused", "done"].includes(t.status));
  const knownTotal = trackable.reduce((sum, t) => sum + num(t.size), 0);
  const downloaded = trackable.reduce(
    (sum, t) => sum + Math.min(num(t.downloaded), num(t.size) || num(t.downloaded)),
    0,
  );
  const remaining = Math.max(0, knownTotal - downloaded);
  $("#stat-progress").textContent = knownTotal
    ? `${bytes(downloaded)} de ${bytes(knownTotal)} · faltam ${bytes(remaining)}`
    : "";
  $("#stat-eta").textContent = total > 0 && remaining > 0 ? `restam ${duration(remaining / total)}` : "";

  // Tempo da sessao (relogio real, nao a soma de conexoes paralelas). Ele para
  // quando a fila para: um cronometro correndo sem nada baixando e ruido.
  const starts = trackable.map((t) => num(t.created_at)).filter((n) => n > 0);
  const active = running.length > 0 || queued > 0;
  const ends = tasks.map((t) => num(t.finished_at)).filter((n) => n > 0);
  const until = active ? Date.now() / 1000 : ends.length ? Math.max(...ends) : 0;
  $("#stat-elapsed").textContent =
    starts.length && until ? `decorrido ${duration(until - Math.min(...starts))}` : "";

  const needed = tasks
    .filter((t) => ["queued", "running", "paused"].includes(t.status))
    .reduce((s, t) => s + Math.max(0, num(t.size) - num(t.downloaded)), 0);

  const diskEl = $("#stat-disk");
  if (freeBytes === null) {
    diskEl.textContent = "";
  } else if (needed > freeBytes) {
    diskEl.textContent = `faltam ${bytes(needed - freeBytes)} de espaço`;
    diskEl.className = "stat is-warn";
  } else {
    diskEl.textContent = `${bytes(freeBytes)} livres`;
    diskEl.className = "stat";
  }
}

async function refreshDisk() {
  try {
    const info = await invoke<{ free: number | null }>("disk_info", { path: null });
    freeBytes = info.free;
    renderStatusBar();
  } catch {
    /* sem espaco lido: a barra de status apenas omite a informacao */
  }
}

// ------------------------------------------------------------------ eventos

function wire() {
  $("#fetch-btn").addEventListener("click", fetchSeries);
  $("#url-input").addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") fetchSeries();
  });

  $("#auth-chip").addEventListener("click", async () => {
    await invoke("open_login", { source: "tiktok" });
    banner("Faça login na janela que abriu. O app detecta sozinho quando terminar.");
  });
  $("#auth-chip-rs").addEventListener("click", async () => {
    await invoke("open_login", { source: "reelshort" });
    banner("Entre na sua conta do ReelShort na janela que abriu, depois volte aqui.");
  });
  $("#auth-chip-cj").addEventListener("click", async () => {
    await invoke("open_login", { source: "candyjar" });
    banner("Entre na sua conta do CandyJar na janela que abriu, depois volte aqui.");
  });

  $("#series-close").addEventListener("click", () => {
    $("#series-panel").hidden = true;
    series = null;
  });

  $("#download-all").addEventListener("click", () => {
    if (!series) return;
    enqueue(series.episodes.filter(available).map((e) => e.number));
  });
  $("#download-sequential").addEventListener("click", () => {
    if (!series) return;
    enqueue(series.episodes.filter(available).map((e) => e.number), 1, true);
  });

  $("#follow-series").addEventListener("click", followCurrent);
  $("#watch-now").addEventListener("click", runWatchNow);
  $("#meta-refresh").addEventListener("click", runMetadata);
  $("#set-auto-interval").addEventListener("input", (e) => {
    $("#set-auto-interval-out").textContent = `${(e.target as HTMLInputElement).value}h`;
  });

  $("#toggle-picker").addEventListener("click", () => {
    const picker = $("#picker");
    picker.hidden = !picker.hidden;
    $("#toggle-picker").setAttribute("aria-expanded", String(!picker.hidden));
  });

  $("#download-selected").addEventListener("click", () => enqueue([...selected]));

  for (const btn of document.querySelectorAll<HTMLElement>("[data-pick]")) {
    btn.addEventListener("click", () => {
      if (!series) return;
      const kind = btn.dataset.pick;
      const usable = series.episodes.filter(available);
      if (kind === "all") setSelection(new Set(usable.map((e) => e.number)));
      else if (kind === "none") setSelection(new Set());
      else if (kind === "missing") {
        const have = haveIds();
        setSelection(
          new Set(
            usable
              .filter((e) => !have.has(episodeKey(e)))
              .map((e) => e.number),
          ),
        );
      }
    });
  }

  // Grade: clique, shift+clique para intervalo, e clique-e-arrasta.
  let dragging = false;
  let dragAdds = true;
  let anchor: number | null = null;

  const grid = $("#ep-grid");
  grid.addEventListener("mousedown", (e) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>(".ep");
    if (!el || el.classList.contains("is-blocked")) return;
    e.preventDefault();
    const n = Number(el.dataset.ep);

    if ((e as MouseEvent).shiftKey && anchor !== null) {
      const next = new Set(selected);
      const [a, b] = anchor < n ? [anchor, n] : [n, anchor];
      for (const ep of series?.episodes ?? []) {
        if (ep.number >= a && ep.number <= b && available(ep)) next.add(ep.number);
      }
      setSelection(next);
      return;
    }

    dragging = true;
    dragAdds = !selected.has(n);
    anchor = n;
    const next = new Set(selected);
    dragAdds ? next.add(n) : next.delete(n);
    setSelection(next);
  });

  grid.addEventListener("mouseover", (e) => {
    if (!dragging) return;
    const el = (e.target as HTMLElement).closest<HTMLElement>(".ep");
    if (!el || el.classList.contains("is-blocked")) return;
    const n = Number(el.dataset.ep);
    const next = new Set(selected);
    dragAdds ? next.add(n) : next.delete(n);
    setSelection(next);
  });

  window.addEventListener("mouseup", () => {
    dragging = false;
  });

  // Acoes das linhas, por delegacao.
  $("#queue-list").addEventListener("click", async (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-act]");
    const row = (e.target as HTMLElement).closest<HTMLElement>(".row");
    if (!btn || !row) return;
    const id = row.dataset.id!;
    const task = tasks.find((t) => t.id === id);
    if (!task) return;

    switch (btn.dataset.act) {
      case "toggle":
        await invoke(
          task.status === "running" || task.status === "queued" ? "task_pause" : "task_resume",
          { id },
        );
        break;
      case "cancel":
        await invoke("task_cancel", { id, deleteFile: false });
        break;
      case "open":
        await invoke("open_path", { path: task.dest });
        break;
      case "prio": {
        const next = task.priority === 2 ? 0 : task.priority === 1 ? 2 : 1;
        await invoke("task_priority", { id, priority: next });
        break;
      }
    }
    refreshQueue();
  });

  $("#pause-all").addEventListener("click", async () => {
    await invoke("queue_pause_all");
    refreshQueue();
  });
  $("#resume-all").addEventListener("click", async () => {
    await invoke("queue_resume_all");
    refreshQueue();
  });
  $("#retry-failed").addEventListener("click", async () => {
    await invoke("queue_retry_failed");
    refreshQueue();
  });
  $("#clear-done").addEventListener("click", async () => {
    await invoke("queue_clear_finished");
    refreshQueue();
  });
  $("#open-folder").addEventListener("click", () =>
    invoke("open_path", { path: settings.dest_dir }),
  );

  // Ajustes.
  $("#open-settings").addEventListener("click", () => {
    $("#settings-modal").hidden = false;
  });
  $("#close-settings").addEventListener("click", () => {
    $("#settings-modal").hidden = true;
  });
  $("#settings-modal").addEventListener("click", (e) => {
    if (e.target === $("#settings-modal")) $("#settings-modal").hidden = true;
  });
  $("#save-settings").addEventListener("click", saveSettings);
  $("#set-conc").addEventListener("input", (e) => {
    $("#set-conc-out").textContent = (e.target as HTMLInputElement).value;
  });
  $("#browse-dest").addEventListener("click", async () => {
    const dir = await openDialog({
      directory: true,
      multiple: false,
      defaultPath: settings.dest_dir || undefined,
    });
    if (typeof dir === "string") $<HTMLInputElement>("#set-dest").value = dir;
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") $("#settings-modal").hidden = true;
  });

  window.addEventListener("focus", refreshAuth);
}

// -------------------------------------------------------------------- inicio

async function main() {
  // Rede de seguranca: um erro solto nao pode mais morrer calado. Antes, uma
  // excecao no render deixava a lista congelada com cara de "esta baixando".
  window.addEventListener("error", (e) => {
    console.error(e.error ?? e.message);
    banner("Algo falhou na interface. A fila continua rodando no fundo.", "is-error");
  });
  window.addEventListener("unhandledrejection", (e) => {
    console.error(e.reason);
  });

  wire();
  await loadSettings();
  await refreshAuth();
  await refreshQueue();
  await refreshFollows();
  await refreshDisk();

  // Uma rodada automatica mexeu na lista: atualiza sem esperar o proximo ciclo.
  await listen<Follow[]>("watch://update", (e) => {
    follows = e.payload;
    renderFollows();
  });
  await listen<WatchReport>("watch://run", (e) => {
    const r = e.payload;
    if (r.added > 0) {
      banner(`${r.added} episódio(s) novo(s) entraram na fila sozinhos.`, "is-ok");
      refreshQueue();
    }
  });

  // A varredura de metadados espera 20s entre uma novela e outra. Sem mostrar
  // em qual pasta esta, o botao ficaria minutos parado parecendo travado.
  await listen<{ done: number; total: number; folder: string }>("meta://progress", (e) => {
    const { done, total, folder } = e.payload;
    $("#meta-status").textContent = folder ? `${done + 1}/${total} — ${folder}` : "";
  });

  // O backend transmite a fila enquanto algo esta rodando.
  let lastEvent = Date.now();
  await listen<Task[]>("queue://update", (e) => {
    lastEvent = Date.now();
    tasks = e.payload;
    renderQueue();
  });

  // Vigia: se o fluxo de eventos parar (listener perdido, webview suspenso ao
  // minimizar para a bandeja) com tarefas ativas, volta a perguntar. A tela
  // nunca fica parada mentindo sobre o que esta acontecendo.
  setInterval(() => {
    const active = tasks.some(
      (t) => t.status === "running" || t.status === "queued" || t.status === "paused",
    );
    if (active && Date.now() - lastEvent > 4000) {
      lastEvent = Date.now();
      refreshQueue().catch((e) => console.error(e));
    }
  }, 2000);

  setInterval(refreshAuth, 15000);
  setInterval(refreshDisk, 10000);
}

main();
