import { cp, mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

const root = process.cwd();
const dist = join(root, "dist");

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

for (const file of ["index.html", "style.css", "renderer.js"]) {
  await cp(join(root, file), join(dist, file));
}

await cp(join(root, "assets"), join(dist, "assets"), { recursive: true });
