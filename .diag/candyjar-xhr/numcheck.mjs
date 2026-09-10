const n = process.argv[2];
const r = await fetch(`https://candyjar.com/pt/series/o-casamento-arranjado-725/episode/${n}`, { headers: { "user-agent": "Mozilla/5.0" } });
const s = await r.text();
const i = s.indexOf("episodeStream");
console.log(`/episode/${n}:`, JSON.parse(`"${s.slice(i, i + 180)}"`).slice(0, 200));
