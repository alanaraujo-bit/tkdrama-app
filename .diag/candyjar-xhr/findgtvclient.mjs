import fs from "node:fs";
for (const f of fs.readdirSync("chunks").filter((f) => f.endsWith(".js"))) {
  const t = fs.readFileSync(`chunks/${f}`, "utf8");
  if (!t.includes("/api/gtv")) continue;
  for (const kw of ["baseUrl", ".Ef)("]) {
    let i = t.indexOf(kw), n = 0;
    while (i !== -1 && n < 5) {
      console.log(`== ${f} :: ${kw}`);
      console.log(t.slice(Math.max(0, i - 420), i + 260).replace(/\n/g, " "));
      i = t.indexOf(kw, i + 1); n++;
    }
  }
}
