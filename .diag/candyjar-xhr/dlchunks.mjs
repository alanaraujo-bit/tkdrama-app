import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
const t = readFileSync(process.argv[2], "utf8");
const outDir = process.argv[3];
mkdirSync(outDir, { recursive: true });

const srcs = new Set();
for (const m of t.matchAll(/<script[^>]+src="([^"]+\.js)"/g)) {
  if (m[1].startsWith("/_next/")) srcs.add(m[1]);
}
const list = [...srcs];
console.log("baixando", list.length, "chunks...");

for (let i = 0; i < list.length; i += 8) {
  const batch = list.slice(i, i + 8);
  await Promise.all(
    batch.map(async (p) => {
      const name = p.replace(/\//g, "_").replace(/%5B/g, "[").replace(/%5D/g, "]");
      try {
        const r = await fetch("https://candyjar.com" + p, {
          headers: { "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126" },
        });
        if (!r.ok) return console.log("FALHOU", r.status, p);
        writeFileSync(outDir + "/" + name, await r.text());
      } catch (e) {
        console.log("ERRO", e.message, p);
      }
    })
  );
}
console.log("ok");
