import { readFileSync } from "node:fs";
const t = readFileSync(process.argv[2], "utf8");
// scripts do Next.js (chunks) com caminho /_next/...
const srcs = new Set();
for (const m of t.matchAll(/<script[^>]+src="([^"]+\.js)"/g)) {
  srcs.add(m[1]);
}
console.log("total chunks:", srcs.size);
// prioriza os que parecem do app (page/_app/main), mas imprime todos
for (const s of [...srcs].sort()) console.log(s);
