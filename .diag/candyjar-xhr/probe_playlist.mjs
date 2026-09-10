// Sonda ao vivo: a "playlist curada por usuario" (v4) devolve o que por episodio?
const url = process.argv[2] ?? "https://candyjar.com/api/gtv/v4/series/725/playlist";
const res = await fetch(url, {
  headers: {
    "user-agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    accept: "application/json",
    "accept-language": "pt-BR,pt;q=0.9,en;q=0.8",
  },
});
console.log("status:", res.status);
const text = await res.text();
import { writeFileSync } from "node:fs";
writeFileSync("playlist_v4_725.json", text);
console.log("saved playlist_v4_725.json", text.length);
try {
  const j = JSON.parse(text);
  console.log("top-level keys:", Object.keys(j));
  if (Array.isArray(j)) console.log("array len:", j.length, "first item keys:", Object.keys(j[0] ?? {}));
  else for (const [k, v] of Object.entries(j)) {
    const s = JSON.stringify(v);
    console.log(`\n## ${k} (${s.length} chars)`);
    console.log(s.slice(0, 900));
  }
} catch (e) {
  console.log("nao e json puro:", text.slice(0, 400));
}
