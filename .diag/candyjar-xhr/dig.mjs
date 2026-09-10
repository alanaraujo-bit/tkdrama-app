import { readFileSync, readdirSync } from "node:fs";
const dir = process.argv[2];
const files = readdirSync(dir).filter((f) => f.endsWith(".js"));
const page = files.find((f) => f.includes("]_page-") && f.includes("episode"));

// contexto amplo ao redor de unlock.unlock no chunk da pagina
const t = readFileSync(`${dir}/${page}`, "utf8");
let i = 0;
while ((i = t.indexOf("unlock.unlock", i)) !== -1) {
  console.log("\n===== unlock.unlock =====");
  console.log(t.slice(Math.max(0, i - 600), i + 500).replace(/\n/g, " "));
  break;
}

// onde w.Y vem de? (import no topo do chunk)
const m = t.match(/w=r\((\d+)\)/);
console.log("\nw = require(", m ? m[1] : "?", ")");

// em todos os chunks: base url / trpc path / fetch(
for (const f of files) {
  const s = readFileSync(`${dir}/${f}`, "utf8");
  for (const pat of [/https?:\/\/[a-z0-9.-]*candyjar[a-z0-9.-]*/gi, /["'`]\/trpc[^"'`]{0,40}/g, /trpcClient|createTRPC|superjson/g]) {
    for (const mm of s.matchAll(pat)) {
      console.log(f + " :: " + mm[0].slice(0, 120));
    }
  }
}
