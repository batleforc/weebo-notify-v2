import { rehypeCodeDefaultOptions } from "fumadocs-core/mdx-plugins";
import { loader } from "fumadocs-core/source";
import { metaSchema, pageSchema } from "fumadocs-core/source/schema";
import { applyMdxPreset } from "fumadocs-mdx/config";
import { defineDocs } from "fumadocs-mdx/macro";
import { docsContentRoute, docsImageRoute, docsRoute } from "./shared";

const docs = defineDocs({
  dir: "content/docs",
  docs: {
    schema: pageSchema,
    postprocess: {
      includeProcessedMarkdown: true,
    },
    // `applyMdxPreset` and not a bare object: a collection's `mdxOptions` is raw
    // `ProcessorOptions`, so passing `rehypeCodeOptions` directly does not type
    // check *and* would silently discard fumadocs' own remark/rehype stack. The
    // preset keeps all of it and layers this on top.
    mdxOptions: applyMdxPreset({
      preset: "fumadocs",
      rehypeCodeOptions: {
        // Spread first so the preset's own themes and transformers survive:
        // `RehypeCodeOptions` requires `themes`, and replacing rather than
        // extending them would silently drop fumadocs' light/dark pair.
        ...rehypeCodeDefaultOptions,
        // A fence whose language Shiki does not ship must not take the site down.
        //
        // It did: a single ```promql block in ops/observability.mdx failed
        // `next build` for all 126 pages with `ShikiError: Language 'promql' not
        // found`. Shiki bundles 723 grammars and promql is not among them, so the
        // whole documentation site could not be released because of one code
        // fence — and nothing about that failure pointed at the fence until you
        // read the stack trace.
        //
        // `fallbackLanguage` makes an unknown language render as plain text
        // instead of throwing, which is the right trade for documentation: the
        // block still renders, and adding a grammar later is an improvement
        // rather than a repair.
        //
        // The fence keeps its `promql` label even though nothing displays it —
        // `addLanguageClass` is off, so no block on this site shows its language.
        // The label is for whoever edits the MDX, and it is what a future grammar
        // would attach to.
        //
        // Kept deliberately as a *fallback* rather than mapping promql onto a
        // lookalike grammar such as `sql`: colouring PromQL with SQL's keywords
        // would highlight the wrong tokens confidently, which is worse than not
        // highlighting it at all.
        fallbackLanguage: "text",
      },
    }),
  },
  meta: {
    schema: metaSchema,
  },
});

// See https://fumadocs.dev/docs/headless/source-api for more info
export const source = loader({
  baseUrl: docsRoute,
  source: docs.toFumadocsSource(),
  plugins: [],
});

export function getPageImageUrl(page: (typeof source)["$inferPage"]) {
  const segments = [...page.slugs, "image.png"];

  return {
    segments,
    url:
      "/" +
      [page.locale, ...docsImageRoute.split("/"), ...segments]
        .filter(Boolean)
        .join("/"),
  };
}

export function getPageMarkdownUrl(page: (typeof source)["$inferPage"]) {
  const segments = [...page.slugs, "content.md"];

  return {
    segments,
    url:
      "/" +
      [page.locale, ...docsContentRoute.split("/"), ...segments]
        .filter(Boolean)
        .join("/"),
  };
}

export async function getLLMText(page: (typeof source)["$inferPage"]) {
  const processed = await page.data.getText("processed");

  return `# ${page.data.title} (${page.url})

${processed}`;
}
