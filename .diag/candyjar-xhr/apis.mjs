import { readFileSync } from "node:fs";
const t = readFileSync(process.argv[2], "utf8");
const s = new Set();
for (const m of t.matchAll(/\/api\/[a-zA-Z0-9_\-/]+/g)) s.add(m[0]);
console.log("endpoints /api/ na pagina:");
console.log([...s].join("\n") || "(nenhum)");

// referencias a chaves de paywall/stream dentro do HTML cru
for (const k of ["unlock", "coin", "purchase", "paywall", "streamUrl", "branchData"]) {
  const hits = [...t.matchAll(new RegExp(k, "g"))].length;
  console.log(`${k}: ${hits} ocorrencias`);
}
