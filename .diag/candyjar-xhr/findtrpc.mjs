import { readFileSync, readdirSync } from "node:fs";
const dir = process.argv[2];
const files = readdirSync(dir).filter((f) => f.endsWith(".js"));

// 1) ocorrencias de trpc com contexto
console.log("===== 'trpc' =====");
for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  let i = 0, n = 0;
  while ((i = t.toLowerCase().indexOf("trpc", i)) !== -1 && n < 4) {
    console.log(f + "::" + t.slice(Math.max(0, i - 100), i + 120).replace(/\n/g, " "));
    i += 4; n++;
  }
}

// 2) fetch com template string (concatenacao de caminho)
console.log("\n===== fetch/template paths =====");
const re = /fetch\(\s*[^)]{0,160}/g;
for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  for (const m of t.matchAll(re)) {
    const s = m[0];
    if (/api|trpc|stream|episode|unlock/i.test(s)) console.log(f + "::" + s.slice(0, 160));
  }
}

// 3) palavras-chave de paywall/unlock com contexto
console.log("\n===== unlock/coin keywords =====");
for (const f of files) {
  const t = readFileSync(`${dir}/${f}`, "utf8");
  for (const kw of ["unlockEpisode", "purchaseEpisode", "redeemCoin", "paywall"]) {
    let i = 0, n = 0;
    while ((i = t.indexOf(kw, i)) !== -1 && n < 3) {
      console.log(f + "::" + kw + " ::" + t.slice(Math.max(0, i - 80), i + 140).replace(/\n/g, " "));
      i += kw.length; n++;
    }
  }
}
