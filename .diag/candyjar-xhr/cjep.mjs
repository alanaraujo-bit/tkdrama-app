import { readFileSync } from "node:fs";
const file = process.argv[2];
const html = readFileSync(file, "utf-8");

const chunks = [];
const re = /self\.__next_f\.push\(\[\d+,"((?:[^"\\]|\\.)*?)"(?:,\s*(true))?\]\)/g;
let m;
while ((m = re.exec(html))) {
  chunks.push(
    m[1]
      .replace(/\\n/g, "\n")
      .replace(/\\t/g, "\t")
      .replace(/\\"/g, '"')
      .replace(/\\\\/g, "\\")
      .replace(/\\u([0-9a-fA-F]{4})/g, (_, h) => String.fromCharCode(parseInt(h, 16)))
  );
}
const blob = chunks.join("");

const br = blob.match(/"branchData":\{[^{}]*?"name":"([A-Z])"/);
console.log("ramo A/B desta sessao:", br ? br[1] : "nao achado");

// todas as ocorrencias de episodeStream: tem isLocked? tem streamUrl?
const streams = [...blob.matchAll(/"episodeStream"\s*:\s*(\{)/g)].length;
console.log("quantidade de 'episodeStream' na pagina:", streams);

// procura os objetos episodeStream completos (contagem de chaves simples)
let idx = 0, count = 0;
while ((idx = blob.indexOf('"episodeStream"', idx + 1)) !== -1) {
  const start = blob.indexOf("{", idx);
  if (start === -1) break;
  let depth = 0, end = -1;
  for (let i = start; i < Math.min(start + 200000, blob.length); i++) {
    if (blob[i] === "{") depth++;
    else if (blob[i] === "}") { depth--; if (depth === 0) { end = i + 1; break; } }
  }
  const obj = blob.slice(start, end);
  count++;
  const lk = /"isLocked"\s*:\s*(true|false)/.exec(obj)?.[1];
  const su = /"streamUrl"\s*:\s*"([^"]{0,90})/.exec(obj)?.[1];
  console.log(`episodeStream #${count}: isLocked=${lk ?? "?"} streamUrl=${su ? su + "..." : "(vazio)"}`);
}

// paywallComponent presente?
console.log("paywallComponent na pagina:", blob.includes("paywallComponent") || blob.includes('"paywall"'));
