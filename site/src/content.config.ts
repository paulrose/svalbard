import { defineCollection } from "astro:content";
import { docsLoader } from "@astrojs/starlight/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

// src/content/docs/docs links to docs/ of the repository, the Markdown that GitHub shows too.
// The pages live under /docs/, so that / stays the splash page.
export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
};
