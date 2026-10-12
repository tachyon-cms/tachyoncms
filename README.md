# TachyonCMS

A headless CMS written in Rust. One binary, one Postgres database, and an API generated from the content types you define.

> [!WARNING]
> TachyonCMS is pre-alpha. The core content API works today. Auth, the admin UI, media, drafts and GraphQL don't exist yet, so don't put it on a public network. Watch the repo to follow along, or jump in.

## Why another CMS?

Pick a CMS today and you usually pick a side. Some are built for developers: typed, API-first, a pleasure to code against, and baffling to the person who has to publish the Tuesday newsletter. WordPress is built for the editor, and you pay for that ease in plugins, patches and page speed.

We think you can have both. Here's what we're building toward:

- **Fast by default.** Rust, Axum and Tokio, with published content read from a single row. We'll publish reproducible benchmarks against the popular CMSs before we claim a single number.
- **Safe by default.** No `unsafe` code, every query either checked at compile time or built from a validated filter tree with bound parameters, and plugins that run in a WebAssembly sandbox with only the permissions you approve.
- **Easy for editors.** An admin that someone who has only used WordPress can pick up without training, from adding a content type to scheduling a post.

## What works today

- Define collections and their fields over the API: text (one value or a list), textarea, number (one or a list), checkbox, date, email, URL, select and radio (with labels), slug, relationship, code, JSON, groups and repeatable arrays, arranged in rows, collapsible sections and tabs that shape the editing screen without changing your data. Every field can carry admin hints such as a description, a placeholder, a sidebar position or a condition for when it shows.
- Create, read, update and delete documents, with validation errors that tell you exactly which field is wrong (`seo.title`, `links.0.url`).
- Paginated lists, newest first.
- Errors in the standard [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) format, each with a request ID you can find in the logs.
- Soft delete, UUIDv7 IDs, and date-times stored in UTC.
- Filter (`where[price][gt]=10`), sort (`sort=-price`) and page through results with cursors; link documents with relationship fields and get them back populated (`depth=2`); edit singleton globals such as site settings.

## Try it

You'll need Rust (stable), Docker, and [`sqlx-cli`](https://crates.io/crates/sqlx-cli) if you plan to change queries.

```bash
git clone https://github.com/tachyon-cms/tachyoncms.git
cd tachyoncms
cp .env.example .env
docker compose up -d
set -a; source .env; set +a
cargo run -p tachyon -- serve
```

The server listens on `127.0.0.1:3000`. Create a content type:

```bash
curl -X POST localhost:3000/api/admin/collections \
  -H 'content-type: application/json' \
  -d '{"slug": "posts", "fields": [
        {"name": "title", "type": "text", "required": true, "maxLength": 120},
        {"name": "publishedOn", "type": "date", "dateOnly": true}
      ]}'
```

Then add a post and list them:

```bash
curl -X POST localhost:3000/api/posts \
  -H 'content-type: application/json' \
  -d '{"title": "Hello, world", "publishedOn": "2026-10-12"}'

curl localhost:3000/api/posts
```

## Changing your content types

Fields change. Maybe "title" should have been "headline", or that "summary" field never earned its keep. Change the schema and TachyonCMS carries your documents across:

```bash
curl -X PUT localhost:3000/api/admin/collections/posts \
  -H 'content-type: application/json' \
  -d '{"schema": {"slug": "posts", "fields": [{"name": "headline", "type": "text"}]},
       "renames": {"title": "headline"}}'
```

Renamed fields keep their data. Anything that would delete or retype data comes back first as a preview (what changes, how many documents it touches, which ones won't validate) and only goes through when you send it again with `"confirm": true`. `POST .../posts/preview` shows the same report without changing anything.

Prefer files under version control? Export every content type, edit the JSON, and apply it:

```bash
cargo run -p tachyon -- schema export -o schema.json
cargo run -p tachyon -- schema diff schema.json --rename posts:title=headline
cargo run -p tachyon -- schema apply schema.json --rename posts:title=headline
```

## Roadmap

| Milestone | What it adds |
|---|---|
| 1 | Schemas, validation, REST CRUD |
| 2 | Filters, sorting, relationships, globals **(done)** |
| 3 | Auth, sessions, API keys, access rules |
| 4 | Drafts, versions, scheduled publishing, Trash |
| 5 | Localization and full-text search |
| 6 | Media library and image processing |
| 7 | GraphQL and OpenAPI |
| 8 | Passkeys, two-factor auth, single sign-on |
| 9 | WebAssembly plugins and webhooks |
| 10 | Live preview |
| 11 | WordPress importer |
| 12 | Release builds, Docker image, Helm chart, benchmarks |

The admin UI is built alongside, starting after milestone 3.

## Project layout

```
crates/tachyon-core   content model and validation (no I/O)
crates/tachyon-db     Postgres migrations and queries
crates/tachyon-api    the HTTP API
crates/tachyon        the binary
```

## Contributing

Bug reports, ideas and pull requests are all welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, tests and how we review. If you've found a security issue, please don't open a public issue. [SECURITY.md](SECURITY.md) explains how to report it privately.

Everyone taking part agrees to our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

[MIT](LICENSE)
