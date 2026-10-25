import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";
import repoLinks from "./src/repo-links.mjs";
import { sidebar } from "./src/pages.mjs";

// brand.mjs writes the Font Awesome logo to brand/ when it has a token. Without it, the site uses
// the original mark in src/assets/.
const brand = existsSync(new URL("./brand/logo.svg", import.meta.url)) ? "./brand" : "./src/assets";

export default defineConfig({
  site: "https://withsvalbard.com",
  // hatch.yml proxies https://svalbard.test to this port.
  server: { port: 7246, allowedHosts: ["svalbard.test"] },
  markdown: { processor: unified({ remarkPlugins: [repoLinks] }) },
  vite: {
    resolve: { alias: { "@brand": fileURLToPath(new URL(brand, import.meta.url)) } },
    // scripts/site.sh dev sets it: file events do not always cross a Docker bind mount.
    server: { watch: process.env.SVALBARD_SITE_POLL ? { usePolling: true, interval: 300 } : {} },
  },
  integrations: [
    starlight({
      title: "Svalbard",
      description: "An open-source CLI for safe database backups.",
      logo: { light: `${brand}/logo-light.svg`, dark: `${brand}/logo-dark.svg` },
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/paulrose/svalbard" }],
      routeMiddleware: "./src/route-data.mjs",
      customCss: ["./src/styles/theme.css"],
      components: {
        ThemeProvider: "./src/components/ThemeProvider.astro",
        ThemeSelect: "./src/components/ThemeSelect.astro",
      },
      plugins: [starlightLinksValidator()],
      sidebar,
    }),
  ],
});
