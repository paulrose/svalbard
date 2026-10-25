import { defineRouteMiddleware } from "@astrojs/starlight/route-data";

// The pages come through the link src/content/docs/docs, which GitHub cannot edit. The edit
// links open the file in docs/ instead.
export const onRequest = defineRouteMiddleware((context) => {
  const route = context.locals.starlightRoute;
  const file = route.entry.filePath?.split("content/docs/docs/")[1];
  if (file) route.editUrl = new URL(file, "https://github.com/paulrose/svalbard/edit/main/docs/");
});
