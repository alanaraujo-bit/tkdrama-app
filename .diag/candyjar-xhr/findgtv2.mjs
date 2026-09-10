import fs from "node:fs";
for (const f of fs.readdirSync("chunks").filter((f) => f.endsWith(".js"))) {
  const t = fs.readFileSync(`chunks/${f}`, "utf8");
  if (!t.includes("(8964)") && !t.includes("(73228)")) continue;
  for (const kw of ["(8964)", "(73228)"]) {
    let i = t.indexOf(kw), n = 0;
    while (i !== -1 && n < 3) {
      console.log(`== ${f} :: ${kw}`);
      console.log(t.slice(Math.max(0, i - 500), i + 200).replace(/\n/g, " "));
      i = t.indexOf(kw, i + 1); n++;
    }
  }
}
