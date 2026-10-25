import favicon from "@brand/favicon.svg?raw";

// From brand/ or src/assets/, like the logo: see astro.config.mjs.
export const GET = () => new Response(favicon, { headers: { "Content-Type": "image/svg+xml" } });
