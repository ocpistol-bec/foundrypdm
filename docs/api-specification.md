# API Specification

HTTP/JSON, bearer-token auth (except `/version`, `/healthz`, `/readyz`, `/auth/login` and the
sign-in page's `/public/branding`, `/public/branding.css` and `/public/branding/logo`). All
tenant-scoped routes operate in the caller's `org_id`. Errors: `4xx/5xx` with body
`{ "error": { "code", "message" } }` (some include structured detail, e.g. check-in conflict).
Base URL in the dev image: `http://<host>:8080`.

## Endpoints (as implemented, `crates/pdm-api/src/routes.rs`)

### Liveness / identity
- `GET /healthz`, `GET /readyz` — probes.
- `GET /version` — public. `{ name, version, build, commit, built_at }`. `build` = test-build tag
  (`PDM_BUILD_TAG`, e.g. `V.01-03`), `commit` baked at build (`PDM_GIT_COMMIT`).
- `POST /auth/login` — `{ username, password, org? }` → `{ token, user, org }`. The token is a web
  sign-in: it ends after an idle spell or at its lifetime (`PDM_SESSION_IDLE_MINUTES`,
  `PDM_SESSION_LIFETIME_MINUTES`). The first request after that gets 401 with code
  `session_expired` and the token is deleted, so later ones get 401 `auth_failed`. Tokens from
  `POST /users/{id}/tokens` never expire on their own.
- `POST /auth/logout` — invalidate the current token.
- `GET /me` — the signed-in identity.
- `GET /me/settings`, `PUT /me/settings` — per-user jsonb payload (`{ payload }`). SolidCraft
  syncs its Prefs here.

### Projects / folders / items
- `GET /projects` → `{ projects: [ {id, name, description, ...} ] }`.
- `POST /projects` `{ name, description? }` → `{ project }`.
- `GET /projects/{id}/items` → `{ items: [ItemSummary] }`. **ItemSummary** includes
  `part_number`, `description`, `classification`, `lifecycle_state`, `doc_id`, `head_version_seq`,
  `head_version`, `head_revision`, `latest_revision`, `modified_by`, `locked_by`, and (new) `filename`, `mime_type`.
- `POST /projects/{id}/items` `{ part_number?, description?, classification?, folder_id? }` → `{ item }`.
- `GET /projects/{id}/folders` → `{ folders: [...] }`. `POST /projects/{id}/folders` `{ name, parent_id? }` → `{ folder }`.
- `PUT /projects/{id}/main-assembly` `{ item_id? }` → `{ project }` — set a project's **main assembly** (the
  item the BOM is built off); `item_id: null` clears it. The item must belong to the project. The
  project's cover image is this item's thumbnail; `Project` responses now include `main_assembly_id`.
- `GET /items/{id}` — item detail (incl. head version + where-used).
- `GET /items/{id}/properties`, `PUT /items/{id}/properties/{key}`.
- `POST /items/{id}/release`.

### Files / versions
- `PUT /blobs` — raw body → `{ blob: { sha256, size_bytes, ... } }` (content-addressed, org-scoped).
- `POST /items/{id}/checkout` `{ workstation? }` → `{ lock, base_version }` (409 if held by another).
- `DELETE /items/{id}/checkout` — undo.
- `POST /items/{id}/checkin` `{ sha256, commit_message?, base_version?, filename?, mime_type?, new_version? }` →
  `{ version }` (409 on stale `base_version`; verifies blob SHA). `new_version` (default `false`)
  controls versioning: a plain save is a new **revision** of the current version (V1 - R2), while
  `new_version: true` starts a new **major version** (V2 - R1). A new major version also flags the
  item's NC programs as outdated; a plain revision does not. **Version** includes `id`, `seq`,
  `version`, `revision`, `file_id`, `author_id`/`author_name`, `commit_message`, `base_version_id`,
  `blob_id`, `sha256`, `size_bytes`, `native_format`, `mime_type`, `measurements`,
  `thumbnail_sha256`/`size`.
- `GET /items/{id}/versions` → `{ versions: [Version] }` (newest last).
- `GET /versions/{id}/blob` — raw bytes + `Content-Digest: sha-256=…`.
- `GET /versions/{id}/thumbnail`, `GET /items/{id}/thumbnail` — PNG preview (**auth-gated**; 401 without
  a token, 404 when none). `PUT /versions/{id}/thumbnail` — store a client-rendered preview (raw PNG body).

### Search / audit
- `GET /search?q=&project?` → `{ results: [ItemSummary] }`.
- `GET /audit?item?` → `{ events: [ {id, action, object_type, actor_id, at, ...} ] }`.

### Admin (Phase-1)
- `POST /users` — create a user. `POST /users/{id}/tokens` — issue a token.
- `PUT /users/{id}` — admin updates a member's `display_name`, `role`, or `active`
  status within the current organization. Set `active: false` to disable access
  and `active: true` to enable it. Disabled users cannot sign in or authenticate
  with existing tokens. Disabling or demoting the last active administrator
  returns `409 Conflict`; this constraint also applies to concurrent updates.

### Branding (`crates/pdm-api/src/branding.rs`)
An organization's display name, logo and color overrides, which replace the "Foundry PDM" mark
and the theme colors. Reads are open to any signed-in caller; writes are admin-only (403
otherwise) and are audited (`org.branding_updated`, `org.branding_logo_set`,
`org.branding_logo_cleared`, `org.branding_reset`, object type `organization`).
- `GET /settings/branding` → **BrandingView**: `{ display_name, show_name_with_logo,
  favicon_from_logo, logo, colors, themes, tokens, signin_org }`. `logo` is `null` or
  `{ url, public_url, mime, width, height, bytes, updated_at }`; the URLs carry `?v=` = the first
  12 hex digits of the logo's SHA-256, and `public_url` is set only when `signin_org` is true.
  `colors` is `{ theme: { token: "#rrggbb" } }`. `themes` (`dark`, `light`, `slate`, `contrast`)
  and `tokens` are the allow-lists the server enforces, so an editor can be built from them.
  `signin_org` says whether the sign-in page shows this organization (see below).
- `PUT /settings/branding` (admin) — partial update `{ display_name?, show_name_with_logo?,
  favicon_from_logo?, colors? }` → BrandingView. An absent field keeps its value;
  `display_name: null` (or blank) clears the name (at most 60 characters, no control or
  invisible formatting characters such as direction marks or zero-width spaces). `colors` merges
  per theme: a token set to `null` drops that override, a theme set
  to `null` drops the theme, `colors: null` drops all overrides. Colors must be `#rrggbb` (stored
  in lower case). An unknown field, theme or token, or a bad value, returns **400** naming the key;
  nothing is saved. Bodies over 64 KiB get **413**.
- `DELETE /settings/branding` (admin) — back to the defaults, logo included → BrandingView.
- `PUT /settings/branding/logo` (admin) — the raw image as the body, `Content-Type: image/png` or
  `image/webp`, at most 256 KiB (**413** above that), 16–2048 px on each side → BrandingView. The
  type is checked from the file's header and must match `Content-Type`; SVG, GIF, JPEG and
  anything else return **400** (convert SVG to PNG before uploading). Replaces any earlier logo.
- `DELETE /settings/branding/logo` (admin) → BrandingView.
- `GET /settings/branding/logo` — the image, with `X-Content-Type-Options: nosniff`,
  `Content-Security-Policy: default-src 'none'; sandbox`, `Cross-Origin-Resource-Policy:
  same-origin`, an `ETag` (`If-None-Match` → 304), and `Cache-Control: private, max-age=31536000,
  immutable` when `?v=` matches the current logo (`private, no-cache` otherwise). 404 when none.

**Public, no token** — what the sign-in page shows. The *sign-in organization* is the one named
by `PDM_SIGNIN_ORG` (see `docs/docker-server.md`), or else the only organization when the server
hosts exactly one; otherwise there is none and these routes answer with the defaults, so one
organization's name and logo are never shown to another's users.
- `GET /public/branding` → `{ display_name, show_name_with_logo, logo_url, favicon_url }` (every
  field `null` without a sign-in organization). Never contains the organization's id or
  registered name. `Cache-Control: no-cache` with an `ETag`.
- `GET /public/branding.css` — `text/css`: the sign-in organization's color overrides, one rule
  per theme, e.g. `html[data-theme="dark"]{--acc:#3b82f6;--on-acc:#0b1220}`; empty without
  overrides. Built by the server from the allow-lists only. `no-cache` with an `ETag`.
- `GET /public/branding/logo` — the sign-in organization's logo, with the same safety headers;
  `Cache-Control: public, max-age=31536000, immutable` when `?v=` matches the current logo,
  `no-cache` otherwise. 404 when there is none.

The 413 answers and malformed-JSON errors on these routes come from the HTTP layer as plain
text, not the `{ "error": … }` shape.

### Reading font (`crates/pdm-api/src/reading_font.rs`)
The organization's default font for the web portal, used for members who have not picked a font
of their own in Settings (a member's own choice wins). Reads are open to any signed-in caller;
writes are admin-only (403 otherwise) and are audited (`org.reading_font_updated`, object type
`organization`).
- `GET /settings/reading-font` → `{ ui, mono }`: `ui` is the face for reading text, `mono` the
  face for identifiers and codes. Each is `system` (the device's own font; the default) or
  `atkinson` (Atkinson Hyperlegible Next / Mono, bundled with the portal under `static/fonts/`).
- `PUT /settings/reading-font` (admin) `{ ui, mono }` → the stored value. Both fields are
  required; any other face or field returns **400** and nothing is saved. The role is checked before
  the body is read (a non-admin always gets 403); bodies over 4 KiB get **413**.

## Gaps (roadmap)
- **Pagination** on list endpoints (currently unbounded).
- **Comments** (`GET/POST /items/{id}/comments`) — schema exists (0004), API not exposed.
- **BOM**, **change orders**, **lifecycle**, **approvals**, **org/team management**, **presence**,
  **WebSocket** events — not yet exposed.
- **API compatibility negotiation** (a `capabilities`/version handshake) — see `docs/development-roadmap.md`.

## Frontend
The web portal (`crates/pdm-api/src/portal.rs`) is a single self-contained HTML/JS page served at `/`
(no build step), authenticated with the same bearer token, calling the endpoints above. A `static/`
dir (or `PDM_PORTAL_DIR`) overrides it when present. The web workspace build-out is a separate effort
(`ToDoWebUI.txt`).
