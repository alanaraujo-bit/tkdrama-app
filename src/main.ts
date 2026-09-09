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

type SourceName = "tiktok" | "reelshort" | "candyjar" | "dramabox";

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
const SOURCE_TAG: Record<string, string> = {
  tiktok: "tk",
  reelshort: "rs",
  candyjar: "cj",
  dramabox: "db",
};

/** Espelha `Source::label()` em media.rs — usado so para textos da tela. */
const SOURCE_LABEL: Record<string, string> = {
  tiktok: "TikTok",
  reelshort: "ReelShort",
  candyjar: "CandyJar",
  dramabox: "DramaBox",
};

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

/** Estado da visão de acompanhadas. Só de tela: nada disso vai para o backend. */
type WatchFilter = "all" | "active" | "paused" | "error";
let watchFilter: WatchFilter = "all";
let watchQuery = "";

/**
 * O portão do agendador está fechado: a rede caiu e a fila parou de puxar
 * tarefa nova de propósito. O backend anuncia as duas bordas por
 * `queue://offline` — uma fila parada por queda precisa se distinguir de uma
 * fila travada por defeito, e às 3h não há ninguém para diferenciar as duas.
 */
let offline = false;

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

/**
 * Um indicador para quatro contas.
 *
 * A barra de topo não é lugar para quatro estados que mudam uma vez por mês.
 * O resumo diz só o que exige ação — "1 conta desconectada" — e o detalhe fica
 * no modal, a um clique. Tudo conectado vira uma linha silenciosa, que é o
 * caso normal.
 */
function renderAccountsChip(entries: Record<string, AuthEntry> | null) {
  const chip = $("#accounts-chip");
  const label = $("#accounts-label");
  if (!entries) {
    chip.className = "chip is-off";
    label.textContent = "contas";
    chip.title = "Não consegui ler as sessões. Clique para ver.";
    return;
  }
  const todas = Object.values(entries);
  const dentro = todas.filter((e) => e.logged);
  const fora = todas.filter((e) => !e.logged);
  if (fora.length === 0) {
    chip.className = "chip is-on";
    label.textContent = `${dentro.length} contas`;
    chip.title = `Sessão ativa em: ${dentro.map((e) => e.label).join(", ")}.`;
  } else {
    chip.className = "chip is-off";
    label.textContent = `${fora.length} fora`;
    chip.title = `Sem sessão em: ${fora.map((e) => e.label).join(", ")}. Clique para entrar.`;
  }
}

async function refreshAuth() {
  const chips: Array<[string, string, string]> = [
    ["tk", "#auth-chip", "#auth-label"],
    ["rs", "#auth-chip-rs", "#auth-label-rs"],
    ["cj", "#auth-chip-cj", "#auth-label-cj"],
    ["db", "#auth-chip-db", "#auth-label-db"],
  ];
  try {
    const r = await invoke<Record<string, AuthEntry>>("auth_status");
    renderAccountsChip(r);
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
    renderAccountsChip(null);
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
    banner(
      "Cole o link de uma novela do TikTok, do ReelShort, do CandyJar ou do DramaBox primeiro.",
      "is-error",
    );
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
    // Antes de desenhar: a grade e o resumo dependem do que a pasta já tem.
    await refreshOnDisk(series.name);
    renderSeries();

    // O aviso que faltava. Colar o link de uma novela que já está no disco não
    // pode parecer um começo do zero — e o caso mais comum é "tenho quase tudo,
    // faltam dois".
    // Os dois avisos abaixo podem valer ao mesmo tempo, e um `banner` sobrescreve
    // o outro: juntá-los é o que impede o segundo de apagar o primeiro.
    const avisos: string[] = [];
    let tudoPronto = false;

    if (onDisk.exists && onDisk.have.size) {
      const faltam = missingEpisodes().length;
      if (faltam === 0) {
        tudoPronto = true;
        avisos.push(
          `"${series.name}" já está completa na pasta: ${onDisk.have.size} episódio(s). Nada a baixar.`,
        );
      } else {
        avisos.push(
          `Você já tem ${onDisk.have.size} episódio(s) de "${series.name}". Faltam ${faltam} — ` +
            `"Baixar tudo" enfileira só esses e não toca no que já está lá.`,
        );
      }
    }

    // O portão de direito do ReelShort e do CandyJar depende da sessão da
    // janela logada. Se sobrou episódio trancado, o motivo mais provável é
    // ela não ter sido capturada — dizer isso é melhor do que deixar o
    // usuário no escuro.
    if (series.source !== "tiktok" && series.episodes.some((e) => e.locked)) {
      const n = series.episodes.filter((e) => e.locked).length;
      const label = SOURCE_LABEL[series.source] ?? series.source;
      avisos.push(
        `${n} episódio(s) ainda trancados. Se a sua conta libera eles, mantenha a janela do ${label} aberta e logada (chip lá em cima) e clique em Buscar de novo.`,
      );
    }

    if (avisos.length) banner(avisos.join(" "), tudoPronto && avisos.length === 1 ? "is-ok" : "");
  } catch (e) {
    banner(String(e), "is-error");
    series = null;
    $("#series-modal").hidden = true;
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
  $("#series-modal").hidden = false;

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
  // O que já está na pasta vem primeiro depois do total: é a informação que
  // decide se esta busca é um download novo ou um complemento.
  if (onDisk.have.size) parts.push(`${onDisk.have.size} já na pasta`);
  if (total > 0) parts.push(bytes(total));
  if (locked) parts.push(`${locked} pago${locked > 1 ? "s" : ""}`);
  if (blocked) parts.push(`${blocked} bloqueado${blocked > 1 ? "s" : ""}`);
  $("#series-stats").textContent = parts.join(" · ");

  // O botão dominante diz o que vai realmente acontecer. "Baixar tudo" numa
  // novela da qual faltam 2 de 84 é uma promessa errada: o app não vai baixar
  // 84, e dizer que vai é o que faz o usuário achar que baixou duas vezes.
  const faltam = missingEpisodes().length;
  const baixarTudo = $<HTMLButtonElement>("#download-all");
  if (faltam === 0) {
    baixarTudo.textContent = "Nada a baixar";
    baixarTudo.disabled = true;
    baixarTudo.title = "Todos os episódios liberados desta novela já estão na pasta.";
  } else {
    baixarTudo.disabled = false;
    baixarTudo.textContent = onDisk.have.size ? `Baixar os ${faltam} que faltam` : "Baixar tudo";
    baixarTudo.title = onDisk.have.size
      ? `${onDisk.have.size} episódio(s) já estão na pasta e não serão baixados de novo.`
      : "";
  }
  $<HTMLButtonElement>("#download-sequential").disabled = faltam === 0;

  renderPicker();
  updatePickSummary();
}

/**
 * Os episódios que já estão na pasta desta novela, lidos do disco.
 *
 * Antes isto vinha da fila ("tarefas concluídas"), que é memória do app e não
 * verdade do disco: bastava *Limpar concluídos* para o app esquecer tudo,
 * apagar os ✓ da grade e "Só os que faltam" voltar a marcar a série inteira.
 * O disco não esquece.
 */
let onDisk: { dir: string; exists: boolean; have: Set<number> } = {
  dir: "",
  exists: false,
  have: new Set(),
};

async function refreshOnDisk(nome: string) {
  try {
    const r = await invoke<{ dir: string; exists: boolean; have: number[] }>("series_on_disk", {
      name: nome,
    });
    onDisk = { dir: r.dir, exists: r.exists, have: new Set(r.have) };
  } catch {
    // Sem leitura do disco a grade apenas deixa de marcar o que já existe. O
    // `enqueue` continua conferindo a pasta do lado do Rust, então nada baixa
    // duas vezes por causa disto.
    onDisk = { dir: "", exists: false, have: new Set() };
  }
}

/** Falta baixar? Um episódio bloqueado não conta como faltando. */
function missingEpisodes(): Episode[] {
  if (!series) return [];
  return series.episodes.filter((ep) => available(ep) && !onDisk.have.has(ep.number));
}

function renderPicker() {
  if (!series) return;
  const grid = $("#ep-grid");
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
      if (onDisk.have.has(ep.number)) b.classList.add("is-have");
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

/** Uma novela passa pelo filtro e pela busca atuais? */
function matchesWatch(f: Follow): boolean {
  if (watchQuery && !f.name.toLowerCase().includes(watchQuery)) return false;
  switch (watchFilter) {
    case "active":
      return !f.paused && !f.error;
    case "paused":
      return f.paused;
    case "error":
      return Boolean(f.error);
    default:
      return true;
  }
}

function renderFollows() {
  $("#watch-next").textContent = settings.auto_enabled
    ? `verificando a cada ${settings.auto_interval_hours}h`
    : "verificação automática desligada";

  const visiveis = follows.filter(matchesWatch);

  // Dois vazios diferentes, e a diferença importa: não acompanhar nada é o
  // primeiro uso e pede uma instrução; filtrar até sobrar nada é um filtro que
  // pede para ser afrouxado. Um texto só mentiria num dos dois casos.
  const vazio = $("#watch-empty");
  vazio.hidden = visiveis.length > 0;
  if (follows.length === 0) {
    vazio.querySelector<HTMLElement>(".empty-title")!.textContent = "Nada sendo acompanhado ainda";
    vazio.querySelector<HTMLElement>(".empty-hint")!.innerHTML =
      'Busque uma novela e clique em <strong>Acompanhar</strong>. A partir daí o ' +
      "app revisita ela sozinho e baixa o episódio novo assim que ele existe.";
  } else if (visiveis.length === 0) {
    vazio.querySelector<HTMLElement>(".empty-title")!.textContent = "Nenhuma novela com esse filtro";
    vazio.querySelector<HTMLElement>(".empty-hint")!.textContent =
      `${follows.length} novela(s) acompanhada(s) — mude o filtro ou limpe a busca.`;
  }

  $("#watch-count").textContent = follows.length
    ? visiveis.length === follows.length
      ? `${follows.length}`
      : `${visiveis.length} de ${follows.length}`
    : "";
  $("#tab-watch-count").textContent = follows.length ? String(follows.length) : "";

  const list = $("#watch-list");
  list.textContent = "";
  for (const f of visiveis) {
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

// ------------------------------------------------------------------- lote

/** Espelha `bulk::Candidate` — o que dá para saber de um link sem gastar rede. */
interface Candidate {
  url: string;
  source: SourceName | null;
  drama_id: string | null;
  following: boolean;
}

/** Espelha `bulk::Outcome`. */
interface BulkOutcome {
  url: string;
  name: string;
  source: SourceName | null;
  episodes: number;
  /** Episódios que já estavam na pasta e não foram baixados de novo. */
  already: number;
  followed: boolean;
  error: string | null;
}

/** Espelha `bulk::Report`. */
interface BulkReport {
  total: number;
  ok: number;
  failed: number;
  episodes: number;
  /** Episódios do lote inteiro que já estavam no disco. */
  already: number;
  followed: number;
  cancelled: boolean;
  /** Por que a rodada parou sozinha (piso de disco), quando parou. */
  stopped: string | null;
  outcomes: BulkOutcome[];
}

/** Espelha o `PACE` de bulk.rs — usado só para estimar o tempo na tela. */
const BULK_PACE_SECS = 20;

let candidates: Candidate[] = [];
let bulkRunning = false;

/** Só os que o app sabe processar. Link não reconhecido fica listado, mas fora. */
function usableCandidates(): Candidate[] {
  return candidates.filter((c) => c.source !== null);
}

/**
 * Relê a caixa e reconta, sem rede.
 *
 * Roda a cada tecla porque a resposta é local e instantânea: ver o número subir
 * enquanto cola é o que dá confiança de que a lista inteira foi entendida.
 */
async function previewBulk() {
  const text = $<HTMLTextAreaElement>("#bulk-input").value;
  const summary = $("#bulk-summary");
  const list = $("#bulk-list");
  const run = $<HTMLButtonElement>("#bulk-run");

  if (!text.trim()) {
    candidates = [];
    summary.hidden = true;
    list.hidden = true;
    run.disabled = true;
    $("#bulk-eta").textContent = "";
    return;
  }

  try {
    candidates = await invoke<Candidate[]>("bulk_preview", { text });
  } catch (e) {
    banner(String(e), "is-error");
    return;
  }

  const uteis = usableCandidates();
  const novos = uteis.filter((c) => !c.following);
  const jaSeguidas = uteis.length - novos.length;
  const desconhecidos = candidates.length - uteis.length;

  // Por fonte: uma lista de 40 vinda de lugares diferentes só faz sentido se
  // der para conferir que cada plataforma trouxe o que devia.
  const porFonte = new Map<string, number>();
  for (const c of uteis) {
    const nome = SOURCE_LABEL[c.source!] ?? c.source!;
    porFonte.set(nome, (porFonte.get(nome) ?? 0) + 1);
  }

  const partes = [`${uteis.length} novela${uteis.length === 1 ? "" : "s"}`];
  if (porFonte.size) {
    partes.push([...porFonte].map(([n, q]) => `${q} do ${n}`).join(" · "));
  }
  if (jaSeguidas) partes.push(`${jaSeguidas} já acompanhada${jaSeguidas > 1 ? "s" : ""}`);
  if (desconhecidos) partes.push(`${desconhecidos} link(s) não reconhecido(s)`);
  summary.textContent = partes.join(" · ");
  summary.hidden = false;

  list.textContent = "";
  for (const c of candidates) {
    const li = document.createElement("li");
    li.className = "bulk-item";
    const tag = document.createElement("span");
    tag.className = "bulk-tag";
    if (c.source) {
      tag.textContent = SOURCE_LABEL[c.source] ?? c.source;
    } else {
      tag.textContent = "?";
      tag.classList.add("is-unknown");
      li.classList.add("is-unknown");
    }
    const link = document.createElement("span");
    link.className = "bulk-url";
    link.textContent = c.url;
    link.title = c.url;
    li.append(tag, link);
    if (c.following) {
      const nota = document.createElement("span");
      nota.className = "bulk-note";
      nota.textContent = "já acompanhada";
      li.append(nota);
    }
    list.append(li);
  }
  list.hidden = candidates.length === 0;

  run.disabled = uteis.length === 0 || bulkRunning;
  run.textContent = uteis.length
    ? `Adicionar ${uteis.length} novela${uteis.length > 1 ? "s" : ""}`
    : "Adicionar";
  // O tempo é a informação que evita a pergunta "travou?". Ele é dominado pela
  // espera entre consultas, não pelo download.
  $("#bulk-eta").textContent = uteis.length
    ? `consulta leva ~${duration(Math.max(0, uteis.length - 1) * BULK_PACE_SECS)}`
    : "";
}

function setBulkBusy(busy: boolean) {
  bulkRunning = busy;
  $<HTMLButtonElement>("#bulk-run").disabled = busy || usableCandidates().length === 0;
  $<HTMLButtonElement>("#bulk-run").textContent = busy ? "Adicionando..." : "Adicionar";
  $("#bulk-cancel").hidden = !busy;
  $("#bulk-progress").hidden = !busy;
  $<HTMLTextAreaElement>("#bulk-input").disabled = busy;
  if (busy) $("#bulk-report").hidden = true;
  else previewBulk();
}

function renderBulkReport(r: BulkReport) {
  const box = $("#bulk-report");
  box.textContent = "";

  const linha = document.createElement("p");
  linha.className = "bulk-report-head";
  const partes = [
    `${r.ok} de ${r.total} adicionada${r.total > 1 ? "s" : ""}`,
  ];
  if (r.episodes) partes.push(`${r.episodes} episódio(s) na fila`);
  // Sem esta linha, um lote de novelas que você já tem apareceria como
  // "0 episódios" e pareceria falha — quando é exatamente o resultado certo.
  if (r.already) partes.push(`${r.already} já estavam na pasta`);
  if (r.followed) partes.push(`${r.followed} acompanhada(s)`);
  if (r.cancelled) partes.push("rodada cancelada");
  linha.textContent = partes.join(" · ");
  box.append(linha);

  // Uma rodada que parou sozinha tem de dizer por quê no lugar mais visível:
  // sem isso, "12 de 40" pareceria falha de rede em vez de disco no limite.
  if (r.stopped) {
    const parada = document.createElement("p");
    parada.className = "setting-hint is-error-text";
    parada.textContent = r.stopped;
    box.append(parada);
  }

  // Só o que falhou vira lista: acertos em massa não precisam ser enumerados,
  // e enumerá-los esconderia justamente as falhas no meio.
  const falhas = r.outcomes.filter((o) => o.error);
  if (falhas.length) {
    const titulo = document.createElement("p");
    titulo.className = "setting-hint";
    titulo.textContent = `${falhas.length} não entrou/entraram:`;
    box.append(titulo);
    const ul = document.createElement("ul");
    ul.className = "bulk-list";
    for (const f of falhas) {
      const li = document.createElement("li");
      li.className = "bulk-item is-fail";
      const nome = document.createElement("span");
      nome.className = "bulk-url";
      nome.textContent = f.name || f.url;
      nome.title = f.url;
      const err = document.createElement("span");
      err.className = "bulk-note is-error-text";
      err.textContent = f.error ?? "";
      li.append(nome, err);
      ul.append(li);
    }
    box.append(ul);
  }
  box.hidden = false;
}

async function runBulk() {
  const urls = usableCandidates().map((c) => c.url);
  if (!urls.length) return;

  const options = {
    download: $<HTMLInputElement>("#bulk-download").checked,
    follow: $<HTMLInputElement>("#bulk-follow").checked,
  };
  if (!options.download && !options.follow) {
    banner("Marque pelo menos uma das duas ações do lote.", "is-error");
    return;
  }

  setBulkBusy(true);
  $("#bulk-bar").setAttribute("style", "width:0%");
  $("#bulk-progress-text").textContent = `preparando ${urls.length} novela(s)...`;

  try {
    const r = await invoke<BulkReport>("bulk_add", { urls, options });
    renderBulkReport(r);
    const resumo =
      `Lote: ${r.ok} de ${r.total} adicionada(s), ${r.episodes} episódio(s) na fila` +
      (r.already ? `, ${r.already} já estavam na pasta.` : ".");
    banner(r.stopped ? `${resumo} ${r.stopped}` : resumo, r.failed || r.stopped ? "is-error" : "is-ok");
    // A caixa esvazia só no sucesso total: com falhas — ou com uma rodada que
    // parou no meio — o texto continua ali para o usuário reprocessar o que
    // sobrou sem ter de recolher os links de novo.
    if (!r.failed && !r.cancelled && !r.stopped) $<HTMLTextAreaElement>("#bulk-input").value = "";
    await refreshFollows();
    await refreshQueue();
  } catch (e) {
    banner(String(e), "is-error");
  } finally {
    setBulkBusy(false);
  }
}

// ----------------------------------------------------------------- vitrine

/** Uma novela vista na vitrine. Espelha o payload de `discover_reelshort`. */
interface Discovered {
  book_id: string;
  name: string;
  cover: string;
  description: string;
  episodes: number;
  /** A partir de qual episódio é pago. 0 = nada anunciado. */
  paid_start: number;
  url: string;
  /** Quantos episódios desta novela já estão na pasta. */
  have_episodes: number;
  following: boolean;
}

let vitrine: Discovered[] = [];
let vitrinePicked = new Set<string>();
let discoverFilter: "new" | "all" = "new";
let discoverQuery = "";

/** Já está na pasta? Um único episódio conta: a novela não é desconhecida. */
function jaTenho(d: Discovered): boolean {
  return d.have_episodes > 0;
}

function visiveisNaVitrine(): Discovered[] {
  return vitrine.filter((d) => {
    if (discoverQuery && !d.name.toLowerCase().includes(discoverQuery)) return false;
    return discoverFilter === "all" || !jaTenho(d);
  });
}

async function refreshDiscover() {
  const btn = $<HTMLButtonElement>("#discover-refresh");
  const status = $("#discover-status");
  btn.disabled = true;
  btn.textContent = "Buscando...";
  status.textContent = "";
  try {
    vitrine = await invoke<Discovered[]>("discover_reelshort");
    vitrinePicked = new Set();
    const novas = vitrine.filter((d) => !jaTenho(d)).length;
    // O texto diz de onde vem e o que não é. Sem isso "105 novelas" se lê como
    // "tudo o que falta no ReelShort", e não é: é a vitrine de hoje.
    status.textContent = `vitrine de hoje · ${vitrine.length} novelas · ${novas} que você não tem`;
    renderDiscover();
  } catch (e) {
    banner(String(e), "is-error");
  } finally {
    btn.disabled = false;
    btn.textContent = "Buscar vitrine";
  }
}

function renderDiscover() {
  const grid = $("#discover-grid");
  const visiveis = visiveisNaVitrine();

  // Três vazios diferentes, e a diferença importa: nunca buscou (instrução),
  // filtrou até sobrar nada (afrouxe o filtro) e já tenho tudo (boa notícia).
  // Um texto só mentiria em dois dos três casos.
  const vazio = $("#discover-empty");
  vazio.hidden = visiveis.length > 0;
  if (vitrine.length === 0) {
    vazio.querySelector<HTMLElement>(".empty-title")!.textContent =
      "Veja o que existe que você ainda não tem";
    // Restaura a instrução: os outros ramos a substituem, e sem isto o estado
    // inicial voltaria com o texto de um filtro vazio.
    vazio.querySelector<HTMLElement>(".empty-hint")!.innerHTML =
      "<strong>Buscar vitrine</strong> lê de uma vez a página inicial do ReelShort e " +
      "mostra as novelas de lá, já descontando as que estão na sua pasta.";
  } else if (discoverFilter === "new" && !discoverQuery) {
    vazio.querySelector<HTMLElement>(".empty-title")!.textContent =
      "Você já tem todas as novelas da vitrine de hoje";
    vazio.querySelector<HTMLElement>(".empty-hint")!.textContent =
      `As ${vitrine.length} em destaque hoje já estão na sua pasta. Amanhã a vitrine muda.`;
  } else {
    vazio.querySelector<HTMLElement>(".empty-title")!.textContent =
      "Nenhuma novela com esse filtro";
    vazio.querySelector<HTMLElement>(".empty-hint")!.textContent =
      `${vitrine.length} na vitrine — mude o filtro ou limpe a busca.`;
  }
  $("#discover-foot").hidden = vitrine.length === 0;
  $("#tab-discover-count").textContent = vitrine.length
    ? String(vitrine.filter((d) => !jaTenho(d)).length)
    : "";
  $("#discover-count").textContent = vitrine.length
    ? `${visiveis.length} de ${vitrine.length}`
    : "";

  grid.textContent = "";
  for (const d of visiveis) {
    const card = document.createElement("button");
    card.type = "button";
    card.className = "disc" + (vitrinePicked.has(d.url) ? " is-sel" : "");
    card.setAttribute("role", "listitem");
    card.dataset.url = d.url;
    card.setAttribute("aria-pressed", String(vitrinePicked.has(d.url)));

    const capa = document.createElement("img");
    capa.className = "disc-cover";
    capa.alt = "";
    capa.loading = "lazy";
    if (d.cover) capa.src = d.cover;
    else capa.hidden = true;

    const nome = document.createElement("span");
    nome.className = "disc-name";
    nome.textContent = d.name;

    const sub = document.createElement("span");
    sub.className = "disc-sub";
    const partes: string[] = [];
    if (d.episodes) partes.push(`${d.episodes} ep`);
    // O que já tenho vem antes do resto: é o que decide se marco ou não.
    if (d.have_episodes) partes.push(`${d.have_episodes} na pasta`);
    if (d.following) partes.push("acompanhando");
    // O que a PLATAFORMA declara, não o que a sua conta libera: a vitrine não
    // consulta conta nenhuma, e quem tem VIP abre muito além disto. Dizer
    // "9 livres" aqui seria afirmar sobre um direito que esta tela não checou.
    if (d.paid_start > 0 && d.episodes >= d.paid_start) {
      partes.push(`pago do E${String(d.paid_start).padStart(2, "0")}`);
    }
    sub.textContent = partes.join(" · ");

    card.title = d.description || d.name;
    card.append(capa, nome, sub);
    if (jaTenho(d)) card.classList.add("is-have");
    grid.append(card);
  }

  const n = vitrinePicked.size;
  // O tamanho é estimativa honesta: o ReelShort não anuncia bytes, e ~380 MB
  // por novela é a média medida na própria biblioteca.
  $("#discover-picked").textContent = n
    ? `${n} selecionada${n > 1 ? "s" : ""} · cerca de ${(n * 0.38).toFixed(1)} GB · ` +
      `${duration(Math.max(0, n - 1) * BULK_PACE_SECS)} só de consulta`
    : "";
  $<HTMLButtonElement>("#discover-add").disabled = n === 0;
  $<HTMLButtonElement>("#discover-add").textContent = n
    ? `Adicionar ${n} novela${n > 1 ? "s" : ""}`
    : "Adicionar selecionadas";
}

/**
 * Manda as marcadas para o mesmo caminho do lote.
 *
 * Reusa `bulk_add` inteiro em vez de um caminho próprio: é o mesmo trabalho —
 * resolver a novela, enfileirar o que falta, acompanhar — e um segundo caminho
 * seria uma segunda chance de divergir no ritmo e no piso de disco.
 */
async function addFromDiscover() {
  const urls = [...vitrinePicked];
  if (!urls.length) return;
  $("#bulk-modal").hidden = false;
  $<HTMLTextAreaElement>("#bulk-input").value = urls.join("\n");
  await previewBulk();
  banner(
    `${urls.length} novela(s) da vitrine prontas no lote. Confira e clique em Adicionar.`,
    "is-ok",
  );
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
  // Sem conexão o rodapé tem de dizer isso antes de qualquer número: "0
  // baixando · 400 na fila" parece o app travado, quando é o app esperando
  // corretamente a internet voltar.
  if (offline) parts.push("sem conexão — esperando a internet voltar");
  if (running.length) parts.push(`${running.length} baixando`);
  if (queued - retrying > 0) parts.push(`${queued - retrying} na fila`);
  if (retrying) parts.push(`${retrying} tentando de novo`);
  if (done) parts.push(`${done} pronto${done > 1 ? "s" : ""}`);
  if (failed) parts.push(`${failed} com falha`);
  $("#stat-counts").textContent = parts.join(" · ") || "ocioso";

  // A aba responde "como está a fila?" mesmo com a outra visão aberta — que é
  // a primeira pergunta ao abrir o app.
  const pendentes = running.length + queued;
  $("#tab-queue-count").textContent = pendentes ? String(pendentes) : "";

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

// ------------------------------------------------------------------ visoes

/**
 * Troca a visão sem desmontar nada.
 *
 * `hidden`, nunca remoção: o render da fila reconcilia as linhas por chave e
 * guarda as referências dos filhos. Tirar `#queue-list` do documento deixaria
 * o mapa cheio de nós órfãos e a fila voltaria a congelar — o defeito mais
 * caro que este app já teve.
 */
function showView(which: "queue" | "watch" | "discover") {
  const abas: Array<["queue" | "watch" | "discover", string, string]> = [
    ["queue", "#tab-queue", "#view-queue"],
    ["watch", "#tab-watch", "#view-watch"],
    ["discover", "#tab-discover", "#view-discover"],
  ];
  for (const [nome, tabSel, viewSel] of abas) {
    const ativa = nome === which;
    const tab = $(tabSel);
    tab.classList.toggle("is-active", ativa);
    tab.setAttribute("aria-selected", String(ativa));
    $(viewSel).hidden = !ativa;
  }
}

/**
 * A conta que decide se "durmo e acordo com tudo baixado" é verdade.
 *
 * Com 20s entre uma novela e outra, o teto de séries por rodada vira minutos de
 * verificação — e nenhum desses dois números aparece junto do outro em lugar
 * nenhum. Sem esta linha, os limites se ajustam no escuro.
 */
function renderAutoMath(s: Settings = settings) {
  const el = $("#auto-math");
  if (!s.auto_enabled) {
    el.textContent = "Com a verificação desligada, nada entra na fila sozinho.";
    return;
  }
  const teto = s.auto_max_new_series;
  const alvo = teto > 0 ? Math.min(teto, follows.length) : follows.length;
  if (alvo === 0) {
    el.textContent = "Nenhuma novela acompanhada ainda — a rodada não tem o que verificar.";
    return;
  }
  // 20s entre uma e outra, sem espera depois da última: é o mesmo ritmo do
  // watch.rs, e é ele que domina o tempo da rodada.
  const consulta = duration(Math.max(0, alvo - 1) * 20);
  const partes = [
    `Cada rodada verifica ${alvo} de ${follows.length} novela(s) — cerca de ${consulta} só de consulta`,
  ];
  if (teto > 0 && follows.length > teto) {
    partes.push(`as ${follows.length - teto} restantes ficam para a rodada seguinte`);
  }
  if (s.auto_max_gb_per_run > 0) {
    partes.push(`e a rodada para ao enfileirar ${s.auto_max_gb_per_run} GB`);
  }
  el.textContent = `${partes.join(", ")}.`;
}

// ------------------------------------------------------------------ eventos

function wire() {
  $("#fetch-btn").addEventListener("click", fetchSeries);
  $("#url-input").addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") fetchSeries();
  });

  // Navegação entre visões.
  $("#tab-queue").addEventListener("click", () => showView("queue"));
  $("#tab-watch").addEventListener("click", () => showView("watch"));
  $("#tab-discover").addEventListener("click", () => showView("discover"));

  // Vitrine. Nada é buscado ao abrir a aba: a consulta é uma ação explícita,
  // porque ela divide a sessão com a fila que pode estar correndo.
  $("#discover-refresh").addEventListener("click", refreshDiscover);
  $("#discover-search").addEventListener("input", (e) => {
    discoverQuery = (e.target as HTMLInputElement).value.trim().toLowerCase();
    renderDiscover();
  });
  for (const btn of document.querySelectorAll<HTMLElement>("[data-discover-filter]")) {
    btn.addEventListener("click", () => {
      discoverFilter = btn.dataset.discoverFilter as "new" | "all";
      for (const outro of document.querySelectorAll<HTMLElement>("[data-discover-filter]")) {
        outro.classList.toggle("is-active", outro === btn);
      }
      renderDiscover();
    });
  }
  // "Marcar todas" marca o que está VISÍVEL, não a vitrine inteira: marcar 105
  // com um filtro aplicado surpreenderia — e aqui cada marca é ~380 MB.
  $("#discover-all").addEventListener("click", () => {
    for (const d of visiveisNaVitrine()) vitrinePicked.add(d.url);
    renderDiscover();
  });
  $("#discover-none").addEventListener("click", () => {
    vitrinePicked = new Set();
    renderDiscover();
  });
  $("#discover-grid").addEventListener("click", (e) => {
    const card = (e.target as HTMLElement).closest<HTMLElement>(".disc");
    if (!card) return;
    const url = card.dataset.url!;
    if (vitrinePicked.has(url)) vitrinePicked.delete(url);
    else vitrinePicked.add(url);
    renderDiscover();
  });
  $("#discover-add").addEventListener("click", addFromDiscover);

  // Contas.
  $("#accounts-chip").addEventListener("click", () => {
    $("#accounts-modal").hidden = false;
    refreshAuth();
  });
  $("#close-accounts").addEventListener("click", () => {
    $("#accounts-modal").hidden = true;
  });
  $("#accounts-modal").addEventListener("click", (e) => {
    if (e.target === $("#accounts-modal")) $("#accounts-modal").hidden = true;
  });

  // Busca e filtro das acompanhadas.
  $("#watch-search").addEventListener("input", (e) => {
    watchQuery = (e.target as HTMLInputElement).value.trim().toLowerCase();
    renderFollows();
  });
  for (const btn of document.querySelectorAll<HTMLElement>("[data-watch-filter]")) {
    btn.addEventListener("click", () => {
      watchFilter = btn.dataset.watchFilter as WatchFilter;
      for (const outro of document.querySelectorAll<HTMLElement>("[data-watch-filter]")) {
        outro.classList.toggle("is-active", outro === btn);
      }
      renderFollows();
    });
  }

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
  $("#auth-chip-db").addEventListener("click", async () => {
    await invoke("open_login", { source: "dramabox" });
    banner("Entre na sua conta do DramaBox na janela que abriu, depois volte aqui.");
  });

  $("#series-close").addEventListener("click", () => {
    $("#series-modal").hidden = true;
    series = null;
  });

  // Só o que falta. O `enqueue` do Rust já recusaria o que está no disco, mas
  // enfileirar 84 para 82 virarem "concluído" no mesmo instante enche a fila de
  // linhas que não são trabalho — e com cinquenta novelas isso é a fila inteira
  // virando ruído.
  $("#download-all").addEventListener("click", () => {
    if (!series) return;
    enqueue(missingEpisodes().map((e) => e.number));
  });
  $("#download-sequential").addEventListener("click", () => {
    if (!series) return;
    enqueue(missingEpisodes().map((e) => e.number), 1, true);
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
        setSelection(new Set(missingEpisodes().map((e) => e.number)));
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

  // Lote.
  $("#open-bulk").addEventListener("click", () => {
    $("#bulk-modal").hidden = false;
    $<HTMLTextAreaElement>("#bulk-input").focus();
    previewBulk();
  });
  const closeBulk = () => {
    // Fechar durante a rodada não a cancela: ela continua no backend e o
    // relatório volta pelo banner. Cancelar é um botão explícito.
    $("#bulk-modal").hidden = true;
  };
  $("#close-bulk").addEventListener("click", closeBulk);
  $("#bulk-modal").addEventListener("click", (e) => {
    if (e.target === $("#bulk-modal")) closeBulk();
  });
  // Uma colagem é um evento só, mas editar dentro de uma caixa com centenas de
  // links seria uma ida ao backend por tecla — e cada ida reescaneia o texto
  // inteiro e clona a lista de acompanhadas. O atraso é curto o bastante para
  // continuar parecendo instantâneo.
  let previewTimer = 0;
  $("#bulk-input").addEventListener("input", () => {
    clearTimeout(previewTimer);
    previewTimer = window.setTimeout(previewBulk, 150);
  });
  $("#bulk-run").addEventListener("click", runBulk);
  $("#bulk-cancel").addEventListener("click", async () => {
    await invoke("bulk_cancel");
    $("#bulk-progress-text").textContent = "cancelando após a novela atual...";
  });

  // Ajustes.
  $("#open-settings").addEventListener("click", () => {
    $("#settings-modal").hidden = false;
    renderAutoMath();
  });
  for (const btn of document.querySelectorAll<HTMLElement>("[data-settings-tab]")) {
    btn.addEventListener("click", () => {
      const alvo = btn.dataset.settingsTab;
      for (const outro of document.querySelectorAll<HTMLElement>("[data-settings-tab]")) {
        const ativa = outro === btn;
        outro.classList.toggle("is-active", ativa);
        outro.setAttribute("aria-selected", String(ativa));
      }
      for (const pane of document.querySelectorAll<HTMLElement>("[data-settings-pane]")) {
        pane.hidden = pane.dataset.settingsPane !== alvo;
      }
      if (alvo === "auto") renderAutoMath();
    });
  }
  // A conta da rodada muda com o que se digita, não só ao salvar: é ajustando
  // que se quer ver o efeito.
  for (const sel of ["#set-auto", "#set-auto-series", "#set-auto-gb", "#set-auto-interval"]) {
    $(sel).addEventListener("input", () => {
      const atual: Settings = {
        ...settings,
        auto_enabled: $<HTMLInputElement>("#set-auto").checked,
        auto_interval_hours: num(Number($<HTMLInputElement>("#set-auto-interval").value)),
        auto_max_new_series: num(Number($<HTMLInputElement>("#set-auto-series").value)),
        auto_max_gb_per_run: num(Number($<HTMLInputElement>("#set-auto-gb").value)),
      };
      renderAutoMath(atual);
    });
  }
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

  // Fechar por clique no fundo: o mesmo gesto vale para todos os modais.
  $("#series-modal").addEventListener("click", (e) => {
    if (e.target === $("#series-modal")) $("#series-modal").hidden = true;
  });

  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    // Fecha só o de cima: com quatro modais, um Esc que fecha tudo apaga
    // trabalho que ainda estava aberto atrás.
    for (const sel of ["#settings-modal", "#accounts-modal", "#bulk-modal", "#series-modal"]) {
      const modal = $(sel);
      if (!modal.hidden) {
        modal.hidden = true;
        return;
      }
    }
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

  // O lote também espera 20s entre uma novela e outra: sem dizer em qual está,
  // a janela ficaria um quarto de hora parecendo travada.
  await listen<{ done: number; total: number; url: string; name: string; eta_secs: number }>(
    "bulk://progress",
    (e) => {
      const { done, total, url, name, eta_secs } = e.payload;
      const pct = total > 0 ? (done / total) * 100 : 0;
      $("#bulk-bar").setAttribute("style", `width:${pct.toFixed(1)}%`);
      $("#bulk-progress-text").textContent =
        `${Math.min(done + 1, total)}/${total} — ${name || url}` +
        (eta_secs > 0 ? ` · faltam ~${duration(eta_secs)}` : "");
    },
  );

  // A varredura de metadados espera 20s entre uma novela e outra. Sem mostrar
  // em qual pasta esta, o botao ficaria minutos parado parecendo travado.
  await listen<{ done: number; total: number; folder: string }>("meta://progress", (e) => {
    const { done, total, folder } = e.payload;
    $("#meta-status").textContent = folder ? `${done + 1}/${total} — ${folder}` : "";
  });

  // Queda e volta da internet. A fila não trava: ela espera, e volta sozinha.
  await listen<boolean>("queue://offline", (e) => {
    offline = e.payload;
    if (offline) {
      banner(
        "Sem conexão. A fila pausou sozinha e volta assim que a internet voltar — nada foi perdido.",
        "is-error",
      );
    }
    renderQueue();
  });
  await listen<boolean>("queue://online", () => {
    offline = false;
    banner("Conexão de volta. A fila retomou de onde parou.", "is-ok");
    refreshQueue();
  });
  // O zelador devolveu à fila o que tinha falhado por motivo passageiro.
  await listen<number>("queue://revived", (e) => {
    if (e.payload > 0) {
      banner(`${e.payload} download(s) que tinham falhado voltaram a tentar sozinhos.`, "is-ok");
      refreshQueue();
    }
  });

  // O backend transmite a fila enquanto algo esta rodando.
  let lastEvent = Date.now();
  // Quantos episódios da novela aberta já terminaram, para saber quando a
  // pasta mudou. Sem isto, o modal continua marcando o estado de antes do
  // download: os ✓ da grade e o "Baixar os N que faltam" ficariam velhos até
  // o usuário buscar a novela de novo — a mesma amnésia que o disco resolveu
  // do outro lado.
  let concluidosDaSerie = -1;
  await listen<Task[]>("queue://update", (e) => {
    lastEvent = Date.now();
    tasks = e.payload;
    renderQueue();

    if (!series || $("#series-modal").hidden) return;
    const prontos = tasks.filter(
      (t) => t.status === "done" && t.drama_id === series!.drama_id && t.source === series!.source,
    ).length;
    if (prontos !== concluidosDaSerie) {
      concluidosDaSerie = prontos;
      refreshOnDisk(series.name).then(() => {
        if (series) renderSeries();
      });
    }
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
