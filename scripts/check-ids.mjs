/**
 * Confere que todo id que `main.ts` procura existe no `index.html`.
 *
 * Existe porque cada acesso ao DOM em main.ts termina em `!` — a asserção de
 * não-nulo do TypeScript. Um id renomeado ou removido no HTML compila sem
 * reclamar, e só quebra na hora em que `wire()` roda: a exceção mata o resto do
 * `main()` e a janela abre vazia, sem erro na tela. Nenhum teste pega isso.
 *
 *   node scripts/check-ids.mjs
 *
 * Sai com código 1 se algum id faltar. Roda em milissegundos, então vale rodar
 * depois de qualquer mexida no HTML.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const raiz = join(dirname(fileURLToPath(import.meta.url)), "..");
const html = readFileSync(join(raiz, "index.html"), "utf8");
const ts = readFileSync(join(raiz, "src", "main.ts"), "utf8");

const declarados = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]));

/** Ids procurados por seletor literal: $("#x"), querySelector("#x"). */
const procurados = new Map();
for (const m of ts.matchAll(/["'`]#([A-Za-z0-9_-]+)["'`]/g)) {
  const linha = ts.slice(0, m.index).split("\n").length;
  if (!procurados.has(m[1])) procurados.set(m[1], linha);
}

/**
 * Seletores por atributo que o HTML precisa oferecer pelo menos uma vez.
 *
 * Estes são piores que um id ausente, não melhores: `querySelectorAll` sem
 * resultado não lança nada — o laço simplesmente não roda, o controle fica
 * inerte, e a build passa verde. Cada seletor de atributo usado em `wire()`
 * tem de estar nesta lista.
 */
const atributos = [
  ['input[name="codec"]', /name="codec"/],
  ["[data-pick]", /data-pick=/],
  ["[data-watch-filter]", /data-watch-filter=/],
  ["[data-settings-tab]", /data-settings-tab=/],
  ["[data-settings-pane]", /data-settings-pane=/],
  ["[data-discover-filter]", /data-discover-filter=/],
];

const faltando = [...procurados].filter(([id]) => !declarados.has(id));
const semAtributo = atributos.filter(([, re]) => !re.test(html));

if (faltando.length === 0 && semAtributo.length === 0) {
  console.log(`ok — ${procurados.size} ids conferidos contra o index.html`);
  process.exit(0);
}

for (const [id, linha] of faltando) {
  console.error(`FALTA  #${id}  (src/main.ts:${linha})`);
}
for (const [sel] of semAtributo) {
  console.error(`FALTA  ${sel}  (nenhum elemento no index.html)`);
}
console.error(
  `\n${faltando.length + semAtributo.length} seletor(es) sem alvo no HTML. ` +
    `A janela abriria vazia e sem erro visível.`,
);
process.exit(1);
