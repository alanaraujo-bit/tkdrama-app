// Sonda: pagina de episodio TRANCADO PROFUNDO (E15 da serie 725).
// Verifica o que o SSR entrega para visitante anonimo: episodeStream,
// branchData, cost/unlockEpisode/paywall.
const url = process.argv[2] ?? "https://candyjar.com/pt/series/o-casamento-arranjado-725/episode/15";
const out = process.argv[3] ?? "cj725e15.html";
const res = await fetch(url, {
  headers: {
    "user-agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    accept: "text/html,application/xhtml+xml",
    "accept-language": "pt-BR,pt;q=0.9,en;q=0.8",
  },
});
console.log("status:", res.status);
const s = await res.text();
import { writeFileSync } from "node:fs";
writeFileSync(out, s);
console.log("saved", out, s.length);

for (const kw of ["episodeStream", "branchData", "unlockEpisode", "\"cost\"", "resolvedBranch", "paywallConfig", "__REMOTE_PAYWALL_CONTEXT__", "actualStreamUrl"]) {
  let i = s.indexOf(kw), n = 0;
  while (i !== -1 && n < 3) {
    console.log(`\n== ${kw} @${i}`);
    console.log(s.slice(Math.max(0, i - 250), i + 450).replace(/\n/g, " "));
    i = s.indexOf(kw, i + 1); n++;
  }
}
