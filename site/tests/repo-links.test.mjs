import test from "node:test";
import assert from "node:assert/strict";
import { forCopies, rewrite } from "../src/repo-links.mjs";

test("links to pages become routes", () => {
  assert.equal(rewrite("restore.md"), "/docs/restore/");
  assert.equal(rewrite("./storage.md#cloudflare-r2"), "/docs/storage/#cloudflare-r2");
  assert.equal(rewrite("index.md"), "/docs/");
});

test("links to other files go to GitHub", () => {
  assert.equal(
    rewrite("../examples/sftp.toml"),
    "https://github.com/paulrose/svalbard/blob/main/examples/sftp.toml",
  );
  assert.equal(
    rewrite("../README.md#license"),
    "https://github.com/paulrose/svalbard/blob/main/README.md#license",
  );
});

test("absolute links and anchors stay", () => {
  for (const url of ["https://example.com/a.md", "/docs/ci/", "#secrets", "mailto:a@b.c"]) {
    assert.equal(rewrite(url), url);
  }
});

test("the Markdown copies keep page links and send other links to GitHub", () => {
  assert.equal(
    forCopies("See [Restore](restore.md) and [`examples/r2.toml`](../examples/r2.toml)."),
    "See [Restore](restore.md) and [`examples/r2.toml`](https://github.com/paulrose/svalbard/blob/main/examples/r2.toml).",
  );
});
