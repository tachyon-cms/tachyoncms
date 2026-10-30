# TachyonCMS

<p>
  <a href="https://github.com/tachyon-cms/tachyoncms/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/tachyon-cms/tachyoncms/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-13233a"></a>
  <a href="https://scorecard.dev/viewer/?uri=github.com/tachyon-cms/tachyoncms"><img alt="OpenSSF Scorecard" src="https://api.scorecard.dev/projects/github.com/tachyon-cms/tachyoncms/badge"></a>
  <a href="https://www.bestpractices.dev/projects/16021"><img alt="OpenSSF Best Practices" src="https://www.bestpractices.dev/projects/16021/badge"></a>
  <a href="https://github.com/tachyon-cms/tachyoncms/discussions"><img alt="Discussions" src="https://img.shields.io/github/discussions/tachyon-cms/tachyoncms?color=e85d0c"></a>
</p>

A headless CMS written in Rust. One binary, one Postgres database, and an API generated from the content types you define.

> [!WARNING]
> TachyonCMS is pre-alpha. The core content API, sign-in and access rules work today. The admin UI, media, drafts and GraphQL don't exist yet, so don't put it on a public network. Watch the repo to follow along, or jump in.

The full documentation, with a step-by-step tutorial and a complete API reference, is at **[docs.tachyoncms.com](https://docs.tachyoncms.com)**.

## Why another CMS?

Pick a CMS today and you usually pick a side. Some are built for developers: typed, API-first, a pleasure to code against, and baffling to the person who has to publish the Tuesday newsletter. WordPress is built for the editor, and you pay for that ease in plugins, patches and page speed.

We think you can have both. Here's what we're building toward:

- **Fast by default.** Rust, Axum and Tokio, with published content read from a single row. We'll publish reproducible benchmarks against the popular CMSs before we claim a single number.
- **Safe by default.** No `unsafe` code, fixed queries checked at compile time, filters and sorts built from a validated tree, every request value a bound parameter, and plugins that run in a WebAssembly sandbox with only the permissions you approve.
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
- Build pages from blocks, and write long-form text as rich text, in JSON or Markdown. Read it back as JSON, HTML, Markdown or plain text with `?format=`. [Pages and rich text](#pages-and-rich-text) shows how.

## Try it

### With Docker

Each [release](https://github.com/tachyon-cms/tachyoncms/releases) ships a signed image. This starts Postgres and TachyonCMS side by side:

```bash
docker network create tachyon
docker run -d --name tachyon-db --network tachyon \
  -e POSTGRES_USER=tachyon -e POSTGRES_PASSWORD=tachyon -e POSTGRES_DB=tachyon \
  postgres:17
docker run --rm --network tachyon -p 127.0.0.1:3000:3000 \
  -e TACHYON_DATABASE_URL=postgres://tachyon:tachyon@tachyon-db:5432/tachyon \
  -e TACHYON_PUBLIC_URL=http://localhost:3000 \
  ghcr.io/tachyon-cms/tachyoncms:v0.1.0
```

### From a release

Download the archive for your platform from the [latest release](https://github.com/tachyon-cms/tachyoncms/releases). There are Linux builds (x86_64 and arm64, static) and macOS builds (Intel and Apple silicon). Then point it at a Postgres database:

```bash
tar xzf tachyon_0.1.0_*.tar.gz
TACHYON_DATABASE_URL=postgres://user:pass@localhost:5432/tachyon ./tachyon_0.1.0_*/tachyon serve
```

The release notes show how to check a download's signature with cosign.

### From source

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

Admins manage users at `/api/users` and roles at `/api/admin/roles`, and custom roles can sit alongside the four built-in ones. A user's email, role and password stay private. Their profile is different: `name` and every other profile field show wherever a document links to them, to anyone who can read that document. An API key needs one more thing: a scope that covers `users`, or the author comes back as a bare ID and `where[author.name]` matches nothing. Got a field that shouldn't travel, like a phone number? Give it a field rule such as `"access": {"read": {"role": "admin"}}` and only Admins will see it in a populated author or be able to filter on it. A role with the `users` ability can change anyone's role, its own included, and make `admin`-scoped keys, so treat it as Admin. The last Admin can't be removed. Passwords need at least 12 characters, and ones known from data breaches are refused. Five wrong passwords in 15 minutes lock an account for 15 minutes.

### Access rules

Roles answer "who are you?". Most real sites also need "which documents?": visitors see what's published, authors see their own drafts and nobody else's. That's what a rule in the schema's `access` says. Rules live in the schema, so they export and apply with it like any other change.

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

Here's a blog where everyone reads what's published and authors also read their own drafts. Authors still change only the posts they created, because the Author role already limits them to that:

```json
{
  "slug": "posts",
  "access": {
    "read": {"or": [{"published": true}, {"author": "$user.id"}]}
  },
  "fields": [
    {"name": "title", "type": "text", "required": true},
    {"name": "published", "type": "checkbox"},
    {"name": "author", "type": "relationship", "relationTo": "users",
     "access": {"update": {"role": "admin"}}}
  ]
}
```

A rule replaces the role defaults for the action it covers, and only that action. Here only `read` has a rule, so the table above still decides `create`, `update` and `delete`: an Author changes only what they created, which the server records itself. The read rule binds Editors too, so they now see only published posts and their own drafts; add `{"role": "editor"}` to its `or` to keep them reading everything. Don't swap the creator check for an `update` rule on `author`: `author` is whatever the client sends, and a create rule can't compare it with the caller, so anyone could name someone else as a post's author. The field rule on `author` stops a byline being reassigned after the fact. Admins pass every rule, so nobody can lock themselves out.

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

A collection or global answers `true`, `false` or `"conditional"`. Conditional means it depends on the document: the rule is a filter, or your role only reaches documents you created. It stays conditional even when every document stored today would pass, because it's an answer about the rules, not the data. Field rules don't depend on the document, so a field answers `true` or `false`, and never allows more than its collection: when you can't update the collection at all, you can't update its fields either. Fields go by their dotted path (`seo.title`, and `links.url` for a field in every row of a list). `schemas` and `users` say whether you may change content types and manage users. Under `collections.users`, `read` is about profiles as links show them, while `update` is about the account routes, where you change only your own unless you manage users. Anything you can't do at all is left out, so a missing collection, global or field means no.

**Checked when you save a schema.** Both the API and `tachyon schema apply` refuse a rule with bad syntax, a role that doesn't exist, a `$user` value that isn't `id`, `role` or a `users` profile field, a filter field or operator that doesn't exist, a `$user` field that doesn't store one value or a list of them (a virtual, join, group or JSON field, say), a list-valued `$user` field with anything but `in` or `not_in`, a filter in a create rule or a field rule, `create` or `delete` on a global, or a rule on a layout field (a row, collapsible or unnamed tab holds no data, so the rule goes on the fields inside). The error names the rule and what's wrong with it. A change that would break a rule somewhere else is refused too, such as removing a profile field another collection's rule reads as `$user.team`. So is deleting a role that a rule names; change the rule first.

**Known limits.**

- A unique field gives one thing away. Saving a duplicate value fails with a 422, even when the document holding that value is one you can't read, so the error tells you some document has it. Don't make a field unique if its values must stay secret.
- A required field that someone can't create, and that has no default, means they can't create the document at all. Give it a default, or let them set it.
- The `users` collection doesn't take collection rules yet; saving one is refused rather than left to do nothing. Rules on its profile fields work.

### Pages and rich text

A landing page isn't one long text field. It's a hero, then a row of features, a quote, a call to action, in whatever order the editor wants this week. That's what a `blocks` field holds: a list of rows, each one of the block types you define, each with its own fields. Long-form writing goes in a `richText` field, and blocks can sit inside that too.

Define a block once and you can use it anywhere. A shared block is a schema of its own, with `"kind": "block"`:

```json
{"slug": "cta", "kind": "block",
 "labels": {"singular": "Call to action", "plural": "Calls to action"},
 "fields": [
   {"name": "label", "type": "text", "required": true},
   {"name": "href", "type": "text"}
 ]}
```

Then name it with `{"blockRef": "cta"}` beside block types written in place:

```json
{
  "slug": "pages",
  "url": "/{slug}",
  "fields": [
    {"name": "title", "type": "text", "required": true},
    {"name": "slug", "type": "slug", "from": "title"},
    {"name": "layout", "type": "blocks", "blocks": [
      {"slug": "hero", "fields": [
        {"name": "heading", "type": "text"},
        {"name": "intro", "type": "richText", "features": ["bold", "italic", "link"]}
      ]},
      {"blockRef": "cta"}
    ]},
    {"name": "body", "type": "richText", "relationTo": ["pages"], "blocks": [
      {"slug": "callout", "fields": [
        {"name": "kind", "type": "select", "options": ["note", "warning"]},
        {"name": "text", "type": "richText"}
      ]},
      {"blockRef": "cta"}
    ]}
  ]
}
```

Each row says which block type it is:

```json
{"title": "Home", "layout": [
  {"blockType": "hero", "heading": "Welcome"},
  {"blockType": "cta", "label": "Get started", "href": "/start"}
]}
```

Every row gets an `id` and keeps it. Lists are replaced whole, so to change one block on a long page, send the whole `layout` back with its `id`s. `minRows` and `maxRows` set how few and how many rows the field takes. Change a shared block and every schema that uses it changes too, with renames carrying the data across. Shared blocks can use other shared blocks, but not in a loop, and you can't delete one while a schema still names it: the 409 says which.

Field rules work inside blocks, and inside rich text's block nodes, exactly as they do in groups and lists. Each row answers to its own block type's rules, so a `secret` that only Editors read in a `hero` row stays readable in a `quote` row with no rule on it. Filters go by path without the block type: `where[layout.heading][equals]=Welcome` searches every row that has a `heading`. `/api/access` lists `layout.heading` the same way, and where block types disagree it answers with what all of them allow.

**What rich text holds.** Paragraphs, headings, bullet, numbered and task lists, quotes, code blocks, tables, horizontal rules and hard breaks; bold, italic, underline, strikethrough, subscript, superscript, inline code and links. A `relationship` node points at a document, and a `block` node holds a row of any block type the field lists. Images are coming with the media library. It's stored as a JSON tree:

```json
{"type": "doc", "content": [
  {"type": "paragraph", "content": [
    {"type": "text", "text": "Takes "},
    {"type": "text", "text": "two", "marks": [{"type": "bold"}]},
    {"type": "text", "text": " minutes."}
  ]}
]}
```

`features` narrows what a field allows (`bold`, `italic`, `underline`, `strike`, `subscript`, `superscript`, `code`, `link`, `heading`, `bulletList`, `orderedList`, `taskList`, `blockquote`, `horizontalRule`, `hardBreak`, `table`, `codeBlock`, `relationship`, `block`); leave it out and everything's allowed. Paragraphs and plain text always are. `relationTo` says which collections links and relationship nodes may point at. A link is either an `href` or a document, `{"doc": {"relationTo": "pages", "value": "<id>"}}`, and `"newTab": true` opens it in a new tab. An `href` has to start with `http://`, `https://`, `mailto:`, `tel:`, `/` or `#`, in any case, so `JavaScript:` and `//evil.example` are refused. One value nests at most 32 levels deep and holds at most 50,000 nodes, counting the rich text inside its block nodes. Anything else comes back as a 422 naming the node, such as `body.content.3.marks.0.attrs.href`.

Rich text can only be filtered on whether it's there: `where[body][exists]=true`. Nothing inside it can be named in a filter, and it can't be sorted on.

Removing a block type, or taking a feature off a field, while documents still use it works like dropping a select option. You get a 409 with a preview that counts the documents that won't validate, with examples, and the change only goes through when you send it again with `"confirm": true`. Stored text is never rewritten behind your back. A row of a removed block type reads back as its `id` and `blockType` alone, to everyone. When you narrow `features`, drop the settings the shorter list can't use: `blocks` needs `block` in the list, and `relationTo` needs `link` or `relationship`. Otherwise the save is refused.

**Writing Markdown.** Send `{"format": "markdown", "value": "..."}` in place of the JSON tree, on collections, globals and users alike, and it's stored as rich text:

```json
{"title": "Clean up", "body": {"format": "markdown", "value": "## Before you start\n\nBack up first. The [install guide](doc:pages/install) has the steps.\n\n:::callout{kind=\"warning\"}\nThis empties the **bin**.\n:::\n\n::cta{label=\"Get started\" href=\"/start\"}\n\n- [x] Read the guide\n- [ ] Run <u>migrate</u>\n"}}
```

That's GitHub-flavoured Markdown (tables, `~~strikethrough~~`, task lists) plus a few extensions, which are also what `?format=markdown` writes:

- `[text](doc:pages/install)` links to the `pages` document whose slug is `install`. You can only name documents you can read; anything else gets the same 422 as a slug that doesn't exist. One write names at most 1,000 documents this way.
- `::ref{collection="pages" slug="install"}` on its own line is a relationship node.
- `::cta{label="Get started" href="/start"}` is a block node. Numbers and checkboxes read as you'd expect (`start="30"` is 30), and a `json` field's value is written as JSON.
- `:::callout{kind="warning"}`, a block of Markdown, then `:::` puts that Markdown in the block's rich text field. It works when the block type has exactly one. Nest one inside another by giving the outer one more colons: `::::callout` around `:::callout`.
- `<u>…</u>` is underline and `<br>` (or `<br/>`, `<br />`) is a hard break. That's all the HTML there is.

Markdown that doesn't fit is refused with a 422 and the line it's on, never half-converted: `line 3: raw HTML isn't allowed; the only tags are <u>…</u> and <br>`, or ``line 3: unknown directive `:::aside` ``. A value can be up to 1 MiB, a numbered list starts at 1,000,000 at most, a NUL character is refused anywhere, and so is a directive straight after a task box (`- [ ] ::cta{…}`). Some things have no Markdown at all: subscript and superscript come out as plain text, and cell spans and `newTab` are dropped.

Markdown can't say everything JSON can, so a few things change on the way through. Each one is deliberate:

- A table cell holding several blocks, or one block that isn't a paragraph, becomes one paragraph, with `<br>` between the blocks.
- Marks come back in a fixed order, and neighbouring text with the same marks becomes one text node.
- Code blocks end their lines with `\n`, never `\r\n` or `\r`.
- An empty paragraph beside other blocks is dropped.
- A numbered list's `"start": 1` and a block node's empty `fields` aren't written.
- A table without a header row gets its first row as the header.
- A task item with no `checked` comes back unchecked.

**Reading it back.** Ask for `?format=html`, `markdown` or `text` on any read of a collection or a global, single or list, and every rich text field comes back as a string, in populated and joined documents too. JSON is the default. `GET /api/users/{id}` takes `format` as well, though not `depth` or `select` (both are a 400 there). Here are two pieces of what `?format=html` makes of that body:

```html
<p>Back up first. The <a href="/install">install guide</a> has the steps.</p>
<div data-block="cta" data-href="/start" data-label="Get started"></div>
```

A link to a document goes to its collection's `url`, here `/{slug}`; without one, it's the link text alone. Block fields become `data-*` attributes (`ctaLabel` is `data-cta-label`), and anything that isn't a plain value is written as JSON. A relationship node is a `<div data-relationship>` around the linked document's first text field. Text is escaped, every `href` is checked again on the way out, and `newTab` links get `target="_blank" rel="noopener noreferrer"`. As `text`, you get the words alone, the rich text inside block nodes included.

What you can't read stays hidden in every format. A field you can't read isn't there, inside block nodes included. A link to a document you can't read is its text alone, and a relationship node pointing at one is left out entirely. In JSON that link stays a bare ID, whatever the `depth`.

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

Done: content types, validation and REST CRUD; filters, sorting, relationships and globals; sign-in, sessions, API keys and access rules. v0.1.0 ships all of it as signed binaries and a Docker image.

Next, in this order, with no dates:

1. Rich text and blocks (in progress)
2. Drafts, versions and scheduled publishing
3. Media library and image processing
4. Live preview
5. Authoring tools: Markdown in and out, bulk import
6. The admin, as easy as WordPress for the people who publish

After that: localization and full-text search, GraphQL and OpenAPI, passkeys, two-factor and single sign-on, WebAssembly plugins and webhooks, a WordPress importer, a Helm chart and published benchmarks. [Discussion #35](https://github.com/tachyon-cms/tachyoncms/discussions/35) has the reasoning, and the polls in [#38](https://github.com/tachyon-cms/tachyoncms/discussions/38) and [#39](https://github.com/tachyon-cms/tachyoncms/discussions/39) can change the order.

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
