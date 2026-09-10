import fs from "node:fs";
for (const f of fs.readdirSync("chunks").filter((f) => f.endsWith(".js"))) {
  const t = fs.readFileSync(`chunks/${f}`, "utf8");
  let i = t.indexOf('cR]'), n = 0;
  while (i !== -1 && n < 3) {
    console.log(`== ${f} :: cR]`);
    console.log(t.slice(Math.max(0, i - 500), i + 200).replace(/\n/g, " "));
    i = t.indexOf("cR]", i + 1); n++;
  }
  const j = t.indexOf('setItem(l.cR');
  if (j !== -1) { console.log(`== ${f} :: setItem(l.cR`); console.log(t.slice(j-600,j+300).replace(/\n/g," ")); }
}
