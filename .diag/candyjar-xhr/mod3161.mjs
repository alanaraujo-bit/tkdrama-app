import { readFileSync, readdirSync } from "node:fs";
const dir = process.argv[2];
const files = readdirSync(dir).filter((f) => f.endsWith(".js"));

// acha em qual chunk o modulo 3161 e definido (webpack: 3161:(...)=>{...})
for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  // padrao de definicao de modulo webpack minificado
  const re = /(?:,|^)3161\s*:\s*\(([^)]*)\)\s*=>\s*\{([\s\S]*?)\}\s*,?\s*(?:\d+\s*:)/;
  for (const m of t.matchAll(/3161\s*:/g)) {
    console.log("\n### chunk " + f + " pos " + m.index);
    console.log(t.slice(m.index, m.index + 40).replace(/\n/g, " "));
  }
}

// procura a definicao real do modulo e imprime um trecho grande
for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  const idx = t.indexOf("3161:");
  if (idx !== -1 && /3161:\s*\(/.test(t.slice(idx, idx + 20))) {
    console.log("\n===== def do modulo 3161 em " + f + " =====");
    console.log(t.slice(idx, idx + 1500).replace(/\n/g, " "));
    break;
  }
}
