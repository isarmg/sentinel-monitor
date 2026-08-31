import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { assertAdministratorWebToolchain } from "@sarmg/admin-web";

const packageJson = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
const lock = JSON.parse(await readFile(new URL("../package-lock.json", import.meta.url), "utf8"));
const nodeVersion = await readFile(new URL("../../../.node-version", import.meta.url), "utf8");
const main = await readFile(new URL("../src/main.tsx", import.meta.url), "utf8");
const styles = await readFile(new URL("../src/styles.css", import.meta.url), "utf8");
const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
const installedPackage = JSON.parse(
  await readFile(new URL("../node_modules/@sarmg/design-tokens/package.json", import.meta.url), "utf8"),
);
const foundationReleaseBase =
  "https://github.com/isarmg/sarmg-foundation/releases/download/v0.3.0";
const foundationPackages = ["admin-web", "contracts", "design-tokens", "http-client"];

assertAdministratorWebToolchain(packageJson, nodeVersion);
for (const name of [
  "react", "react-dom", "@types/react", "@types/react-dom",
  "vite", "@vitejs/plugin-react", "typescript",
]) {
  const declared = packageJson.dependencies?.[name] ?? packageJson.devDependencies?.[name];
  assert.equal(lock.packages[`node_modules/${name}`]?.version, declared);
}

for (const name of foundationPackages) {
  const dependency = `@sarmg/${name}`;
  const expected = `${foundationReleaseBase}/sarmg-${name}-0.3.0.tgz`;
  assert.equal(packageJson.dependencies[dependency], expected);
  assert.equal(lock.packages[""].dependencies[dependency], expected);

  const locked = lock.packages[`node_modules/${dependency}`];
  assert.equal(locked?.version, "0.3.0");
  assert.equal(locked?.resolved, expected);
  assert.match(locked?.integrity ?? "", /^sha512-[A-Za-z0-9+/]+={0,2}$/);
}
assert.equal(installedPackage.version, "0.3.0");
for (const name of ["tokens.css", "reset.css", "accessibility.css"]) {
  assert.match(main, new RegExp(`import "@sarmg/design-tokens/${name.replace(".", "\\.")}";`));
}
const expectedDigests = {
  "tokens.css": "124b788529faf5031ff7b12ac7c5493a1ceb3d11c76693bfa7e5d971f22547d4",
  "reset.css": "54556e5d22e275fe9aafdaca468056d17e09da3de93729637c0f2481a8f26eab",
  "accessibility.css": "8153af37ecc40a69c1305f8179777e0c60b1f2a730bf3ebfcebe43aecf9df0bb",
};
for (const [name, expected] of Object.entries(expectedDigests)) {
  const content = await readFile(
    new URL(`../node_modules/@sarmg/design-tokens/dist/${name}`, import.meta.url),
  );
  assert.equal(createHash("sha256").update(content).digest("hex"), expected);
}
assert.doesNotMatch(main, /vendor\/sarmg-design/);
assert.match(main, /useAdministratorSession/);
assert.match(main, /@sarmg\/contracts/);
assert.match(main, /@sarmg\/http-client/);
assert.match(main, /createRoot/);
assert.match(main, /name="username"/);
assert.match(main, /session\.username/);
assert.doesNotMatch(main, /name="email"|session\.email|user\.email|管理员邮箱/);
assert.doesNotMatch(main, /operator|viewer|data-role/);
assert.match(html, /<body\s+data-sarmg-scope>/);
for (const token of [
  "--sarmg-action-primary",
  "--sarmg-bg-page",
  "--sarmg-bg-panel",
  "--sarmg-text-primary",
  "--sarmg-text-danger",
  "--sarmg-font-sans",
]) {
  assert.match(styles, new RegExp(`${token}:`));
}

console.log("Sentinel Foundation 设计边界验证通过");
