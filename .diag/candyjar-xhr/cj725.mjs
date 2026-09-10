import { readFileSync } from "node:fs";
const html = readFileSync(process.env.TEMP + "\\cj725.html", "utf-8");

// decodifica os pedacos self.__next_f.push([1,"..."]) como o modulo Rust faz
const chunks = [];
const re = /self\.__next_f\.push\(\[\d+,"((?:[^"\\]|\\.)*?)"(?:,\s*(true))?\]\)/g;
let m;
while ((m = re.exec(html))) {
  // unescape de string JS (o decode do Rust trata as sequencias comuns)
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

// branchData (ramo A/B desta sessao)
const br = blob.match(/"branchData":\{[^{}]*?"name":"([A-Z])"/);
console.log("ramo A/B desta sessao:", br ? br[1] : "nao achado");

// objetos de episodio
const objs = [...blob.matchAll(/\{[^{}]*"episodeNumber"\s*:\s*\d+[^{}]*\}/g)].map((x) => x[0]);
const seen = new Map();
for (const o of objs) {
  const n = parseInt(/"episodeNumber"\s*:\s*(\d+)/.exec(o)[1], 10);
  const lk = /"isLocked"\s*:\s*(true|false)/.exec(o)?.[1] ?? "-";
  const bl = /"isBlocked"\s*:\s*(true|false)/.exec(o)?.[1] ?? "-";
  const su = o.includes("streamUrl");
  seen.set(n, { lk, bl, su });
}
const rows = [...seen.entries()].sort((a, b) => a[0] - b[0]);
console.log("total episodios na lista:", rows.length);
let firstLocked = null;
for (const [n, v] of rows) {
  const locked = v.lk === "true" || v.bl === "true";
  if (locked && firstLocked == null) firstLocked = n;
  console.log(
    `E${String(n).padStart(2, "0")} isLocked=${v.lk.padEnd(5)} isBlocked=${v.bl.padEnd(5)}${v.su ? " <== tem midia" : ""}`
  );
}
console.log("primeiro trancado (fronteira):", firstLocked);
