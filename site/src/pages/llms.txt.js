import guide from "../llms.md?raw";
import { copyPath, groups, site, text } from "../llms.mjs";

const list = (entries) =>
  entries.map((e) => `- [${e.data.title}](${site}${copyPath(e.id)}): ${e.data.description}`).join("\n");

export async function GET() {
  const all = await groups();
  // The last two groups, the safety contract and development, are long and optional for setup.
  const main = all.slice(0, -2).flatMap((group) => group.entries);
  const optional = all.slice(-2).flatMap((group) => group.entries);
  return text(`${guide.trim()}

## Docs

- [All pages in one file](${site}/llms-full.txt): every page below, as Markdown.
${list(main)}

## Optional

${list(optional)}
`);
}
