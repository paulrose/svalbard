// llms.txt, llms-full.txt and a Markdown copy of each page, for language models. llms.md is the
// written part of llms.txt; the lists of pages come from docs/.
import { getCollection } from "astro:content";
import { sidebar } from "./pages.mjs";
import { forCopies } from "./repo-links.mjs";

export const site = "https://withsvalbard.com";

/** The path of the Markdown copy of a page, such as /docs/install.md. */
export const copyPath = (id) => `/docs/${id.replace(/^docs\/?/, "") || "index"}.md`;

/** A page as Markdown: its title and description, then its text. */
export const markdown = (entry) =>
  `# ${entry.data.title}\n\n> ${entry.data.description}\n\n${forCopies(entry.body ?? "").trim()}\n`;

/** The pages in the groups and the order of the sidebar. */
export async function groups() {
  const entries = new Map((await getCollection("docs")).map((entry) => [entry.id, entry]));
  return sidebar.map(({ label, items }) => ({ label, entries: items.map((id) => entries.get(id)) }));
}

export const text = (body, type = "text/plain") =>
  new Response(body, { headers: { "Content-Type": `${type}; charset=utf-8` } });
