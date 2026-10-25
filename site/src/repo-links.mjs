// Rewrites the relative links of docs/*.md, which work on GitHub, for the site. A link to a page
// becomes its route, and a link to any other file of the repository goes to that file on GitHub.
const docs = "https://github.com/paulrose/svalbard/blob/main/docs/";

export function rewrite(url) {
  if (/^([a-z][a-z0-9+.-]*:|\/|#)/i.test(url)) return url;
  const [path, hash = ""] = url.split(/(?=#)/);
  const page = path.match(/^(?:\.\/)?([^/]+)\.md$/);
  if (page) return page[1] === "index" ? `/docs/${hash}` : `/docs/${page[1]}/${hash}`;
  return new URL(url, docs).href;
}

export default function repoLinks() {
  const walk = (node) => {
    if (node.type === "link" || node.type === "definition") node.url = rewrite(node.url);
    node.children?.forEach(walk);
  };
  return walk;
}

// For the Markdown copies of the pages (see llms.mjs): links to pages stay relative, so they open
// the copies, and links to other files of the repository go to GitHub.
export function forCopies(markdown) {
  return markdown.replace(/\]\(\.\.\/([^)\s]+)\)/g, (_, path) => `](${new URL(`../${path}`, docs).href})`);
}
