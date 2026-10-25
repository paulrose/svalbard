// Writes brand/ with the Svalbard logo, a Font Awesome Pro icon, when FONTAWESOME_API_TOKEN is
// set. The Pro licence does not allow the icon in an open-source repository, so it never enters
// the repository: scripts/site.sh runs this in its container, and only the built site holds the
// icon. Without the token, the site uses the original mark in src/assets/.
import { mkdir, writeFile } from "node:fs/promises";

const token = process.env.FONTAWESOME_API_TOKEN?.trim();
if (!token) {
  console.log("brand: FONTAWESOME_API_TOKEN is not set, so the site uses src/assets/");
  process.exit(0);
}

const api = "https://api.fontawesome.com";
const auth = await fetch(`${api}/token`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}` },
});
if (!auth.ok) {
  throw new Error(
    `Font Awesome rejected FONTAWESOME_API_TOKEN: HTTP ${auth.status}. It must be an API token, with the Pro SVG scope.`,
  );
}
const { access_token: access } = await auth.json();

const query = `{ release(version: "7.3.1") { icon(name: "mountains") {
  svgs(filter: { familyStyles: [{ family: SHARP_DUOTONE, style: REGULAR }] }) { html } } } }`;
const response = await fetch(api, {
  method: "POST",
  headers: { Authorization: `Bearer ${access}`, "Content-Type": "application/json" },
  body: JSON.stringify({ query }),
});
const { data, errors } = await response.json();
const html = data?.release?.icon?.svgs?.[0]?.html;
if (!html) throw new Error(`Font Awesome sent no SVG: ${JSON.stringify(errors ?? data)}`);

const licence =
  "<!--!Font Awesome Pro 7.3.1 by @fontawesome - https://fontawesome.com License - https://fontawesome.com/license (Commercial License)-->";
const inner = html.replace(/^<svg[^>]*>/, "").replace(/<\/svg>\s*$/, "");
const shapes = inner.includes("Font Awesome") ? inner : licence + inner;
// The shapes fill x 1 to 637 and y 81 to 544 of the 640 x 640 canvas.
const logo = (fill) =>
  `<svg xmlns="http://www.w3.org/2000/svg" fill="${fill}" viewBox="0 80 640 465">${shapes}</svg>\n`;
const favicon = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" rx="16" fill="#203d3c" />
  <g fill="#f3f0e6" transform="translate(6.5 7) scale(0.08)">${shapes}</g>
</svg>
`;

const dir = new URL("./brand/", import.meta.url);
await mkdir(dir, { recursive: true });
await Promise.all([
  writeFile(new URL("logo.svg", dir), logo("currentColor")),
  writeFile(new URL("logo-light.svg", dir), logo("#243c38")),
  writeFile(new URL("logo-dark.svg", dir), logo("#e4e9d9")),
  writeFile(new URL("favicon.svg", dir), favicon),
]);
console.log("brand: wrote the Font Awesome logo to brand/");
