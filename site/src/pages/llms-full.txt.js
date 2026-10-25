import { groups, markdown, text } from "../llms.mjs";

export async function GET() {
  const pages = (await groups()).flatMap((group) => group.entries);
  return text(pages.map(markdown).join("\n"));
}
