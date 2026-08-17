# weebo-notify documentation

The published documentation site: architecture, development environment, operations,
the API reference and the [Architecture Decision Records](./content/docs/adr).

Built with [Fumadocs](https://fumadocs.dev) as a **static export** — the output is
plain files, with local search, and no runtime service to host
([ADR-0010](./content/docs/adr/0010-fumadocs-for-the-project-documentation.mdx)).

```bash
task dev:docs        # development server on http://localhost:3000
pnpm -C docs build   # static export into docs/out/
```

## Layout

| Path | What lives there |
| --- | --- |
| `content/docs/` | The published pages (MDX) |
| `content/docs/api/openapi.json` | **Generated** by `task recu` — do not edit |
| `src/` | The Next.js application: layout, search route, MDX components |
| `../docs/.prompt/` | The source design document — deliberately *not* published |

`.prompt/` sits outside the content tree on purpose: it is the specification the
product is built from, not documentation for the people using it.

## Writing a page

Add an `.mdx` file under `content/docs/`, with `title` and `description` in the
frontmatter. Ordering and section titles live in the neighbouring `meta.json`.

For a decision record, use the generator rather than writing the file by hand — it
handles the numbering and the template:

```bash
task adr -- "the decision title"
```
