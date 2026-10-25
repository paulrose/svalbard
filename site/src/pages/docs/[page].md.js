import { getCollection } from "astro:content";
import { copyPath, markdown, text } from "../../llms.mjs";

// /docs/install.md and so on: the Markdown of each page, which llms.txt links.
export async function getStaticPaths() {
  return (await getCollection("docs")).map((entry) => ({
    params: { page: copyPath(entry.id).slice("/docs/".length, -".md".length) },
    props: { entry },
  }));
}

export const GET = ({ props }) => text(markdown(props.entry), "text/markdown");
