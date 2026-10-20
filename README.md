# TachyonCMS

A headless CMS written in Rust. One binary, one Postgres database, and an API generated from the content types you define.

> [!WARNING]
> TachyonCMS is pre-alpha. The core content API, sign-in and access rules work today. The admin UI, media, drafts and GraphQL don't exist yet, so don't put it on a public network. Watch the repo to follow along, or jump in.

## Why another CMS?

Pick a CMS today and you usually pick a side. Some are built for developers: typed, API-first, a pleasure to code against, and baffling to the person who has to publish the Tuesday newsletter. WordPress is built for the editor, and you pay for that ease in plugins, patches and page speed.

We think you can have both. Here's what we're building toward:

- **Fast by default.** Rust, Axum and Tokio, with published content read from a single row. We'll publish reproducible benchmarks against the popular CMSs before we claim a single number.
- **Safe by default.** No `unsafe` code, every query either checked at compile time or built from a validated filter tree with bound parameters, and plugins that run in a WebAssembly sandbox with only the permissions you approve.
- **Easy for editors.** An admin that someone who has only used WordPress can pick up without training, from adding a content type to scheduling a post.

## What works today

- Define collections and their fields over the API: text (one value or a list), textarea, number (one or a list), checkbox, date, email, URL, select and radio (with labels), slug, relationship (to one collection or several), code, map points you can search by distance (`near` returns the nearest first and pages with `page`), JSON, groups and repeatable arrays, arranged in rows, collapsible sections and tabs that shape the editing screen without changing your data. Every field can carry admin hints such as a description, a placeholder, a sidebar position or a condition for when it shows.
- Sign in with a session cookie or a bearer token, or use scoped API keys. Four roles (Admin, Editor, Author, Viewer) decide who can change what, and you can add your own.
- Access rules in the schema narrow that per collection, global and field: published posts for everyone, drafts for their authors, a salary field for HR. `GET /api/access` tells each caller what they may do.
- Create, read, update and delete documents, with validation errors that tell you exactly which field is wrong (`seo.title`, `links.0.url`).
- Send only what changed. An update to `seo.title` leaves the rest of `seo` alone, lists are replaced whole, and `null` clears a field wherever it sits (inside a `json` field it's just a value).
- Date-times are stored in UTC to the millisecond, so what you filter on is exactly what was saved.
- Paginated lists, newest first.
- Errors in the standard [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) format, each with a request ID you can find in the logs.
- Soft delete, UUIDv7 IDs, and date-times stored in UTC.
- Filter (`where[price][gt]=10`), including through relationships (`where[author.name][equals]=Ann`) and into list rows (`where[links.url][like]=docs`), sort (`sort=-price`) and page through results with cursors; link documents with relationship fields and get them back populated (`depth=2`); edit singleton globals such as site settings.
- Ask for only what you need. `select[title]=true` returns just the title (and `id`), `populate[authors][name]=true` trims the documents your links fill in, and a collection's `defaultPopulate` sets that trim once. A relationship's `maxDepth` stops population going deeper than you want, whatever `depth` a request asks for.
- Virtual fields show a value from a linked document without storing it: a field with `"virtual": "author.name"` reads the author's name on every read, and you can filter on it like any other field.
- Join fields list the documents that link to this one. Give a category `{"type": "join", "collection": "posts", "on": "category"}` and every read shows its posts, newest first. Page, sort, filter and count them per request with `joins[posts][limit]=5&joins[posts][sort]=-title`, or switch them off with `joins=false`. A join inside a group goes by its path, `joins[meta.recent]` or `joins[meta][recent]`.

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

The server listens on `127.0.0.1:3000`. Until someone runs setup it logs `setup needed`, and nothing can be changed.

### First run

Create the first Admin. This works once; after that, setup refuses.

```bash
curl -X POST localhost:3000/api/setup \
  -H 'content-type: application/json' \
  -d '{"email": "you@example.com", "name": "You", "password": "a long passphrase you will remember"}'
```

Sign in. A browser gets a session cookie; from the command line, ask for a bearer token instead and keep it:

```bash
curl -X POST localhost:3000/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"email": "you@example.com", "password": "a long passphrase you will remember", "bearer": true}'

export TOKEN=tcs_...   # the "token" from the response
```

Create a content type:

```bash
curl -X POST localhost:3000/api/admin/collections \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"slug": "posts", "fields": [
        {"name": "title", "type": "text", "required": true, "maxLength": 120},
        {"name": "publishedOn", "type": "date", "dateOnly": true}
      ]}'
```

Add a post, then list posts. Reading needs no sign-in:

```bash
curl -X POST localhost:3000/api/posts \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"title": "Hello, world", "publishedOn": "2026-10-12"}'

curl localhost:3000/api/posts
```

For a website or a server that writes content, make an API key scoped to what it needs. The key is shown once:

```bash
curl -X POST localhost:3000/api/auth/keys \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name": "website", "scopes": ["posts:read"]}'
```

### Who can do what

Unless an access rule says otherwise, anyone can read, and signed-in users write according to their role:

| Role | Can |
|---|---|
| Viewer | read |
| Author | create content, and change or delete what they created |
| Editor | change all content and globals |
| Admin | everything, including content types, users and roles |

Admins manage users at `/api/users` and roles at `/api/admin/roles`. A user's email, role and password stay private, but their profile fields (such as `name`) show wherever another document links to them, so keep private details out of profile fields; custom roles can be added alongside the four built-in ones. The last Admin can't be removed. Passwords need at least 12 characters, and ones known from data breaches are refused. Five wrong passwords in 15 minutes lock an account for 15 minutes.

### Access rules

Roles answer "who are you?". Most real sites also need "which documents?": visitors see what's published, authors edit their own posts and nobody else's. That's what a rule in the schema's `access` says. Rules live in the schema, so they export and apply with it like any other change.

You can put them in three places:

- on a collection, for `read`, `create`, `update` and `delete`;
- on a global, for `read` and `update`;
- on a field at any depth, inside groups, lists and tabs too, for `read`, `create` and `update`.

A rule is one of these:

- `true` or `false`;
- `{"role": "editor"}`, or a list such as `{"role": ["editor", "reviewer"]}`, custom roles included;
- `{"signedIn": true}` (or `false`);
- `{"and": [rules]}` or `{"or": [rules]}`;
- any other object, which is a filter written exactly as `where` takes it: `{"published": {"equals": true}}`, or `{"published": true}` for short. Filters reach through relationships (`{"author.team": "red"}`) and into lists, just as `where` does.

A filter can compare with the person asking. `$user.id`, `$user.role` and `$user.` plus a `users` profile field (`$user.team`) stand in for their values. The profile field has to store one value or a list of them; virtual and join fields are filled on read, not stored, so rules can't use them. A list works with `in` and `not_in` only. For someone who isn't signed in there's no `$user`, so a condition that uses it matches nothing.

Here's a blog where everyone reads what's published, and authors read their own drafts and change only their own posts:

```json
{
  "slug": "posts",
  "access": {
    "read": {"or": [{"published": true}, {"author": "$user.id"}]},
    "update": {"author": "$user.id"}
  },
  "fields": [
    {"name": "title", "type": "text", "required": true},
    {"name": "published", "type": "checkbox"},
    {"name": "author", "type": "relationship", "relationTo": "users"}
  ]
}
```

A rule replaces the role defaults for the action it covers, and only that action. Here `create` and `delete` have no rule, so the table above still decides them. `read` and `update` do, and they bind Editors too: they now see only published posts and their own drafts, and change only posts they wrote. To keep them working on everything, add `{"role": "editor"}` to the read rule's `or`, and make the update rule `{"or": [{"role": "editor"}, {"author": "$user.id"}]}`. Admins pass every rule, so nobody can lock themselves out.

**How it's enforced.**

- Reads: the rule's filter goes into the database query wherever a document can appear. That covers lists and single reads, references, joins, populated links (one you can't read stays a bare ID) and virtual fields (no value from a document you can't read). It covers filters through relationships too, so nobody can probe hidden documents with `where[author.team]`. A document you can't read is a 404. A list the rule refuses outright, such as `false` or a role you don't have, answers 401 when you're signed out and 403 when you're not.
- Updates and deletes are checked against the stored document inside the write. If you can't read the document it's a 404; if you can read it but the rule says no, it's a 403. When the rule refuses every document, such as a role you don't have, the answer is a 401 or 403 straight away. A global's existence is no secret, so an update its rule refuses is always a 403, never a 404.
- A create rule can only use roles, sign-in and `true`/`false`, since there's no document to compare yet. A refused create is a 401 or 403.
- If a write succeeds but you can't read the result, the response holds only its `id`.
- A new link to a document you can't read gets the same 422 as a link to one that doesn't exist.
- A delete refused because other documents link to it is a 409. Only an Admin whose credential isn't narrowed by key scopes sees how many; everyone else just hears that something does.
- Field rules can only use roles, sign-in and `true`/`false` for now. A field you can't read is left out of every response, populated and joined documents included. Filtering or sorting on it is a 403 that names the parameter. Sending it in a write you're not allowed (even as `null`) is a 403 that names the field.
- API keys stay narrowed by their scopes on top of every rule.

**What am I allowed to do?** `GET /api/access` answers for whoever asks, so an admin screen can hide the buttons that would fail. Here's an Author under the blog rules above, trimmed to one field and one global:

```json
{
  "collections": {
    "posts": {
      "read": "conditional", "create": true, "update": "conditional", "delete": "conditional",
      "fields": {"title": {"read": true, "create": true, "update": true}}
    }
  },
  "globals": {
    "site": {"read": true, "update": false, "fields": {"tagline": {"read": true, "update": false}}}
  },
  "schemas": false,
  "users": false
}
```

A collection or global answers `true`, `false` or `"conditional"`. Conditional means it depends on the document: the rule is a filter, or your role only reaches documents you created. It stays conditional even when every document stored today would pass, because it's an answer about the rules, not the data. Field rules don't depend on the document, so a field answers `true` or `false`, and never allows more than its collection: when you can't update the collection at all, you can't update its fields either. Fields go by their dotted path (`seo.title`, and `links.url` for a field in every row of a list). `schemas` and `users` say whether you may change content types and manage users. Anything you can't do at all is left out, so a missing collection, global or field means no.

**Checked when you save a schema.** Both the API and `tachyon schema apply` refuse a rule with bad syntax, a role that doesn't exist, a `$user` value that isn't `id`, `role` or a `users` profile field, a filter field or operator that doesn't exist, a `$user` field that doesn't store one value or a list of them (a virtual, join, group or JSON field, say), a list-valued `$user` field with anything but `in` or `not_in`, a filter in a create rule or a field rule, `create` or `delete` on a global, or a rule on a layout field (a row, collapsible or unnamed tab holds no data, so the rule goes on the fields inside). The error names the rule and what's wrong with it. A change that would break a rule somewhere else is refused too, such as removing a profile field another collection's rule reads as `$user.team`. So is deleting a role that a rule names; change the rule first.

**Known limits.**

- A unique field gives one thing away. Saving a duplicate value fails with a 422, even when the document holding that value is one you can't read, so the error tells you some document has it. Don't make a field unique if its values must stay secret.
- A required field that someone can't create, and that has no default, means they can't create the document at all. Give it a default, or let them set it.
- The `users` collection doesn't take collection rules yet; saving one is refused rather than left to do nothing. Rules on its profile fields work.

### Putting it online

Set `public_url` to the address people use, such as `https://cms.example.com`, and serve it over HTTPS. Set it behind a reverse proxy too, even though the server itself listens on `127.0.0.1`: browser sign-ins compare against it. The server won't start on a public address without it, because browser sign-ins only accept changes coming from that address. Other settings live under `[auth]` in `tachyon.toml` (see `tachyon.example.toml`), or in the environment as `TACHYON_AUTH__SETTING`.

## Changing your content types

Fields change. Maybe "title" should have been "headline", or that "summary" field never earned its keep. Change the schema and TachyonCMS carries your documents across:

```bash
curl -X PUT localhost:3000/api/admin/collections/posts \
  -H "authorization: Bearer $TOKEN" \
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
