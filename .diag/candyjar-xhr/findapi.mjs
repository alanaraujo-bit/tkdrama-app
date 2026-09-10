import { readFileSync, readdirSync } from "node:fs";
const dir = process.argv[2];
const files = readdirSync(dir).filter((f) => f.endsWith(".js"));
const seen = new Map(); // caminho -> [arquivo, contexto]

for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  for (const m of t.matchAll(/["'`](\/api\/[a-zA-Z0-9_\-/${}.]*)["'`]/g)) {
    const path = m[1];
    if (!seen.has(path) || seen.get(path).length < 3) {
      const ctx = t.slice(Math.max(0, m.index - 90), m.index + path.length + 60);
      seen.set(path, [...(seen.get(path) ?? []), `${f}: ${ctx.replace(/\n/g, " ")}`]);
    }
  }
}

const sorted = [...seen.entries()].sort((a, b) => a[0].localeCompare(b[0]));
console.log("total endpoints unicos:", sorted.length);
for (const [p, ctxs] of sorted) {
  console.log("\n== " + p);
  for (const c of ctxs.slice(0, 2)) console.log("   " + c);
}
