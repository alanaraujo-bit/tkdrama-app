import fs from "node:fs";
for (const f of fs.readdirSync("chunks").filter((f) => f.endsWith(".js"))) {
  const t = fs.readFileSync(`chunks/${f}`, "utf8");
  for (const kw of ["visitId", "randomUUID"]) {
    let i = t.indexOf(kw), n = 0;
    while (i !== -1 && n < 2) {
      console.log(`== ${f} :: ${kw}`);
      console.log(t.slice(Math.max(0, i - 450), i + 350).replace(/\n/g, " "));
      i = t.indexOf(kw, i + 1); n++;
    }
  }
}
