# degen-portal — plan

degen-tools for social. Same shape (`run`, `skill`, `list`, dashboard + local JSON API for
agents, declarative HTTP tool packages), different auth problem: X and Discord don't take a
static key out of a `.env`, and a post is irreversible and public.

Goal: an agent runs `degen-portal run x_post --text "..."` or hits `POST /v1/run` and a tweet
appears, without the agent ever holding a token, and without it being able to post 200 times
because it got a 429 and retried.

**Personal tool, not a platform.** One operator, N accounts you own. No users table, no tenants,
no sessions, no sign-up, no hosted web UI, and above all no shared X app paying for strangers'
posts. Everything lives in `~/.degen-portal` on your machine; the only multi-ness is several
accounts per provider. Anything that would exist only to serve other people is out of scope by
construction — that deletes the broker, the database, and the deploy.
It runs when you run it, on the machine in front of you. No deploy, no always-on daemon, no
second machine.

## 1. What carries over from degen-tools, and what doesn't

Reused as-is (`~/ai/degen-tools/src`):

| Module | Role here |
|---|---|
| `package.rs`, `tool.rs` | metalcraft `integration.json` + `api_tools/*.json` format, `$NAME` expansion limited to `requires_env`, `allowed_hosts` |
| `paths.rs` | `data.id`, `images[].url` response paths — used for `secret_paths`, `save`, and the new `post_id_path` |
| `run.rs` | build request → send → mask → save media/secrets |
| `server.rs` | 127.0.0.1-only, per-run bearer token, Origin/Host rejection. Copy the lockdown verbatim |
| `tui.rs`, `skill.rs`, `install.rs`, `project.rs` | dashboard, agent-facing docs, third-party packages, per-project `.env` scoping |

Does not carry over:

- **Static credentials.** `config::lookup_credential(name) -> Option<String>` is a pure store
  lookup. An X bearer token lives 2 hours and must be refreshed under a lock, and there can be
  three X accounts on one machine.
- **`body_mapping`.** Only `none | params | params_nested | template` are implemented
  (`tool::SUPPORTED_BODY_MAPPINGS`). Discord attachments and X `media/upload/append` need
  `multipart`. That is a real work item, not a config flag.
- **Fire-and-forget calls.** Neon calls are idempotent-ish and private. `x_post` is neither.

### Code sharing decision

Extract a `degen-core` workspace crate inside the degen-tools repo (`package`, `tool`, `paths`,
`run`, `config`, `errors`, masking) and depend on it by path/git from degen-portal.

Not a fork-and-copy: the masking and host-allowlist code is the security surface of both
binaries, and a bug fixed in one copy will not get fixed in the other. Cost is one afternoon of
restructuring degen-tools; it keeps `cargo test` green in one place.

The extraction needs exactly one new abstraction:

```rust
// degen-core
pub trait CredentialResolver {
    /// Value for a `$NAME` a package declared in requires_env.
    fn resolve(&self, name: &str, ctx: &CallContext) -> Result<Option<String>, DegenError>;
}
```

degen-tools implements it over `.env` + `credentials.json` + process env (today's behaviour).
degen-portal implements it over the same store **plus** the OAuth account store, where
`$OAUTH_TOKEN` resolves to a live access token for `ctx.account`, refreshing first if it expires
within 60s. `tool::expand_env`'s `requires_env` allowlist stays exactly as-is — a third-party
social package still can't name a credential it didn't declare.

## 2. No backend. Investigated, and neither provider needs one

Infra cost: **$0**. The earlier relay design was solving a problem that isn't there.

**X accepts a loopback callback.** The proof is X's own official CLI,
[xurl](https://github.com/xdevplatform/xurl): its built-in default `REDIRECT_URI` is
`http://localhost:8080/callback`, and it runs a local listener, binding **both `127.0.0.1` and
`::1`** because browsers resolve `localhost` either way. If X's portal refused loopback URIs, X's
own tool would not ship that default. The "127.0.0.1 is rejected" reports are about the *Website
URL* field, not the callback field.

Fallback ladder, if one app's settings ever balk:
1. `http://portal.localtest.me:7719/oauth/callback` — `*.localtest.me` publicly resolves to
   127.0.0.1 (verified: `dig +short portal.localtest.me` → `127.0.0.1`). Looks like a hostname to
   the validator, hits your laptop. No `/etc/hosts` edit, no server.
2. Headless mode — print the authorize URL, approve on any device, paste the redirect URL back.
   No listener at all, so it also covers boxes with no browser.

**Discord needs even less.** Adding a bot is, in Discord's own words, "a special server-less and
callback-less OAuth2 flow": `https://discord.com/oauth2/authorize?client_id=…&scope=bot&permissions=…`
with **no `response_type` and no `redirect_uri`** — "Bot authorization does not require these
parameters because there is no need to retrieve the user's access token"
([docs](https://docs.discord.com/developers/topics/oauth2)). Posting authenticates with the bot
token from the portal. Listening, later, is a gateway WebSocket — an *outbound* connection.

The one Discord feature that would require a public HTTPS server is the Interactions Endpoint URL
(Discord POSTs slash commands to you, Ed25519-signed). Using gateway-delivered interactions
removes that requirement entirely: "If your app is using Gateway-based interactions, you don't
need to configure an Interactions Endpoint URL."

So there is no server in this design at all — not for auth, and (below) not for hosting either.

## 3. Auth per provider

### X — OAuth 2.0 authorization code + PKCE, user context

- Authorize: `https://x.com/i/oauth2/authorize`, scopes `tweet.read tweet.write users.read
  media.write offline.access` (`media.write` is required for *every* v2 media endpoint under
  PKCE, alt text included). `dm.write` only when the user opts in per account.
- Token: `POST https://api.x.com/2/oauth2/token`. Public client ⇒ `client_id` in the body, no
  secret. Access token 2h; `offline.access` yields a refresh token; treat rotation as mandatory
  (persist the new refresh token in the same atomic write that stores the access token).
- Refresh: single-flight per account (`tokio::sync::Mutex` per account id) so five parallel agent
  calls don't burn five refreshes and race a rotating token.
- Redirect URI: `http://localhost:7719/oauth/callback`, registered exact-match in the portal;
  listen on `127.0.0.1` and `::1` both.
- Headless: print the URL, paste the redirect back. The auth code expires **30 seconds** after
  approval, so for a remote box prefer connecting on the laptop and `accounts import`.
- Portal gotcha worth writing into the error message: if `/2/*` returns `client-forbidden` or
  `client-not-enrolled` *after* a successful auth, the app must be moved to the **Pay-per-use**
  package and the **Production** environment in the developer console. xurl documents the same
  fix; it is not a local bug, and it will otherwise eat an afternoon.

### Discord — bot token; OAuth only to install, and that flow is callback-less

Posting as a *user* account is a selfbot and is a ToS violation ("refraining from automating
standard user accounts"). So:

- **Bot token** (`Authorization: Bot <token>`) is the credential for
  `POST /channels/{id}/messages`. It is static ⇒ it reuses degen-tools' credential store
  completely untouched. No OAuth code runs on the Discord path at all.
- **Install** is `degen-portal discord invite --guild <id>`: prints the callback-less authorize
  URL, you click it, the bot joins. Leave **Public Bot unchecked** in the app settings so only you
  can add it anywhere — the correct setting for a personal tool.
- **Webhooks** as the zero-permission path: a webhook URL is a bearer secret scoped to one
  channel. Fastest route to an agent posting today; store it as an ordinary credential.
- Reading message *content* needs the privileged Message Content intent. v1 skips it: poll
  `GET /channels/{id}/messages?after=` as a normal tool. Gateway comes in P6 if wanted.

## 4. Data model

```
~/.degen-portal/
  accounts.json        0600, atomic rename (copy config::save_credentials)
  servers/<port>.json  0600, for `connect`
  posts.db             append-only audit log (JSONL is fine; no sqlite dependency yet)
```

```jsonc
// accounts.json
{ "apps": { "main": { "client_id": "…", "redirect_uri": "http://localhost:7719/oauth/callback" } },
  "accounts": {
    "x:degenspartan": {
      "provider": "x", "handle": "degenspartan", "user_id": "1234",
      "scopes": ["tweet.write", "..."],
      "access_token": "...", "expires_at": 1789..., "refresh_token": "...",
      "app": "main",
      "policy": { "max_per_hour": 10, "max_per_day": 40, "approval": "auto|queue" }
    },
    "discord:degenbuilders": { "provider": "discord", "kind": "bot",
      "guild_ids": ["..."], "channel_allow": ["1234"], "token": "..." }
} }
```

Account selection, mirroring degen-tools' project scoping: `--account x:degenspartan` >
`PORTAL_X_ACCOUNT` in the project `.env` > the sole connected account for that provider > error
listing candidates. Never silently pick account #1 when there are two — wrong-account posting is
the expensive failure.

## 5. Package/tool format extensions

Additive fields the metalcraft runner ignores, same discipline as degen-tools' `save` /
`secret_paths` / `allowed_hosts`:

| Field | Meaning |
|---|---|
| `auth` | `{ "provider": "x", "scopes": ["tweet.write"] }` — the runner injects a live token for the selected account and fails early if the account lacks a scope |
| `writes` | `"public"` — the call is externally visible; subject to the safety layer in §6 |
| `post_id_path` | `data.id` — what the audit log records, and what `x_delete_post` undoes |
| `idempotency` | `["text","reply_to"]` — parameters hashed for the duplicate window |
| `rate_class` | `post` / `read` / `upload` — which budget the call debits |

`multipart` body mapping added to `tool.rs` for uploads (file path parameter → streamed part,
never inlined into a log line).

## 6. The part degen-tools doesn't have: a safety layer

An agent that retries is a normal agent. An agent that retries `x_post` is a spam incident. Every
tool with `"writes": "public"` goes through:

1. **Duplicate suppression.** SHA-256 of (`account`, `idempotency` params); identical hash inside
   15 min returns the *original* post id with `"deduped": true` instead of posting again.
2. **Budget.** Per-account `max_per_hour` / `max_per_day`, refused with the reset time in the
   error. Also a hard `--max-spend` estimate for X's per-post pricing.
3. **No blind retries.** 401 → refresh once → retry once (a 401 means the write never happened).
   429 or 5xx → **never** retried automatically; the error says so, in the words the agent reads.
4. **Approval queue** (`policy.approval = "queue"`). The call returns `{"queued": id}`; the TUI
   shows the draft, `a` approves, `d` drops. Default `auto` for Discord, `queue` for X on first
   connect.
5. **Target allowlists.** Discord: `channel_allow` per account — an agent cannot discover a
   channel id and post in #announcements. X: `dm.write` off unless enabled.
6. **Audit log + undo.** Every public write appends `{ts, account, tool, args_digest, post_id,
   permalink}`. `degen-portal log` lists; `degen-portal undo <id>` deletes via the provider's
   delete endpoint.

Masking rules extend to tokens: an access/refresh token is never printed, `auth list` shows
`x:degenspartan  expires in 47m  scopes tweet.write,media.write`.

## 7. Tools

**x** (`packages/x/api_tools/`): `x_me`, `x_post` (text, `reply_to`, `quote_id`, `poll`,
`media_ids`), `x_thread` (n posts chained, each debiting the budget), `x_delete_post`,
`x_upload_media` (initialize/append/finalize + STATUS poll behind one tool; `media.write`),
`x_get_post`, `x_search_recent`, `x_list_mentions`, `x_like`, `x_repost`, `x_follow`,
`x_send_dm` (opt-in).

**discord** (`packages/discord/api_tools/`): `discord_send_message` (content, embeds,
`reply_to`, `thread_id`), `discord_send_webhook`, `discord_edit_message`, `discord_delete_message`,
`discord_upload_attachment`, `discord_get_messages`, `discord_list_guilds`, `discord_list_channels`,
`discord_create_thread`, `discord_add_reaction`, `discord_bot_invite_url`.

Each package ships `skills/<id>.md` written for an agent, and it must say in the first paragraph:
*a post is public and permanent; if a call fails, read the error, do not call again.*

## 8. Agent interface

Identical ergonomics to degen-tools, own port and namespace so both can run side by side:

```bash
eval "$(degen-portal connect)"          # DEGEN_PORTAL_URL=http://127.0.0.1:7719, DEGEN_PORTAL_TOKEN=...
curl -s $DEGEN_PORTAL_URL/v1/run -H "Authorization: Bearer $DEGEN_PORTAL_TOKEN" \
  -d '{"tool":"x_post","account":"x:degenspartan","args":{"text":"gm"}}'
```

New endpoints beyond degen-tools': `GET /v1/accounts`, `POST /v1/accounts/connect`
(returns the authorize URL — the *human* clicks it, never the agent), `DELETE /v1/accounts/{id}`,
`GET /v1/log`, `GET|POST /v1/queue/{id}` (approve/drop), `GET /v1/budget`.

CLI: `degen-portal connect x`, `... accounts`, `... run`, `... skill`, `... log`, `... undo`,
`... revoke <account>`.

## 9. Phases

Each phase ends in something demonstrable, not a compiling skeleton.

**P0 — `degen-core` extraction. DONE.** `degen-tools` is now a workspace: `core/` holds
`package`, `tool`, `paths`, `run`, `config`, `project`, `args`, `auth`, `install`, `skill` and
`server`; the binary keeps `main`, `cli`, `tui`, its `packages/` and its agent overview. What
differs between binaries is one struct, `degen_core::App` (name, version, state dir, env prefix,
user agent, bundled packages, credential resolver, call policy, overview), set by `init()` before
anything reads state. *Verified:* 37 tests, same count as before the split; `degen-tools list`,
`serve`, `connect`, `/v1/status`, `/v1/tools/{name}` and the Origin refusal all behave as they
did.

**P1 — Discord. DONE.** `degen-portal` binary: 11 tools (`discord_send_message`, `_edit_`,
`_delete_`, `_get_messages`, `_add_reaction`, `_create_thread`, `_list_guilds`, `_list_channels`,
`_get_channel`, `_me`, `_send_webhook`), bot token and webhook URL as ordinary stored
credentials, `discord invite` printing the callback-less URL, and the channel allowlist.
*Verified:* a write to an unallowed channel is refused by CLI and by `POST /v1/run`; after
`discord allow`, the same call reaches `discord.com/api/v10` and returns the API's own 401 for a
bogus token; reads are never gated; 8 tests.

**P2 — OAuth core + X posting. DONE.** `oauth.rs`: PKCE (S256, RFC 7636), a loopback listener on
both `127.0.0.1` and `::1` at port 7720, state validation, the token exchange, rotating-refresh
persistence, and a lock file around refresh so two processes cannot race X's rotation and strand
the account. `credentials.rs` resolves `$X_ACCESS_TOKEN` from the connected account, refreshing
when it is inside a 60s margin. 11 X tools; `connect x` (browser or `--headless` paste),
`accounts`, `accounts default`, `accounts revoke` (revokes at X, then forgets locally), and
`run --account x:handle`. *Verified:* 25 tests, including the RFC test vector, a real browser
callback caught and answered over TCP, an expired token refreshed against a local token endpoint
with the rotated refresh token persisted, a live token used without a refresh, and a connected
account arriving at a server as `Authorization: Bearer <token>` — end to end, with no X
credentials needed.

Not done in P2, by design: media (P4), and any limit on how much an agent may post (P3).

**P3 — Safety layer. DONE.** One append-only ledger (`~/.degen-portal/ledger.jsonl`) answers all
three questions that need the same facts: did I already post this, how much have I posted, and
how do I take it back. A call publishes when its tool declares `post_id_path`; edits, deletes and
reactions are written down but neither counted nor deduplicated. Gates, cheapest first: the
Discord channel allowlist, a 15-minute repeat window, hourly and daily caps (X 10/20, Discord
30/200 by default), and an optional approval queue where a call is held instead of sent. Nothing
is ever retried automatically. New commands: `log`, `undo`, `queue`, `approve`, `drop`, `budget`,
`approval`. *Verified:* 38 tests. The acceptance cases assert against a server that counts what
arrives: two identical posts produce **one** request, an exhausted budget refuses with the
minutes until a slot frees, a queued call never reaches the wire until `approve`, and a dropped
one never does at all. Also proven at the CLI end to end — post, refused repeat, `log`, `undo`
issuing the DELETE.

Core grew two seams for it: `CallPolicy::check` now returns a `Verdict` (`Send` or `Hold`, so a
policy can answer without calling out) and `CallPolicy::record` runs after every non-GET call.

**P4 — Media. DONE for images; video deferred, see below.** Core gained the `multipart` body
mapping: `file_params` names the arguments that are paths, `param_paths` renames a parameter to
the form field an API wants (Discord's `files[0]`), and `payload_json_field` packs the remaining
arguments into one JSON field (Discord's `payload_json`). Files are read with a 512 MB ceiling
and a content type from the extension, because an API that checks the type rejects
`application/octet-stream`. Tools: `x_upload_media` (X's simple upload, images and GIFs, then
`x_post --media_ids`) and `discord_upload_attachment` (file and message in one request).
*Verified:* 46 tests. Three core unit tests on the split, three wire tests against a server that
parses the body — field names, `filename`, `Content-Type: image/png`, the PNG's own bytes, the
boundary, and that the tool's own `Content-Type` header does not survive to break it. An upload
to an unallowed channel is refused like any other write, and a missing file fails before
anything is sent. Also demonstrated at the CLI: upload, then a post carrying `media.media_ids`.

**X video is not implemented, and it is not a small gap.** It needs four calls —
`initialize`, `append` per 5 MB chunk, `finalize`, then poll `STATUS` until processing ends.
Every tool here is exactly one HTTP request, by design: that is what makes a package declarative
JSON that the metalcraft agent can also run. Supporting video means a composite tool — a Rust
implementation registered beside the JSON ones and dispatched by `run` and `/v1/run`. That is a
real change to what a tool *is*, so it is a decision to take deliberately rather than smuggle in
under "media". Images cover posting charts, screenshots and art, which is the actual use.

**P5 — Optional, only if wanted.** Discord gateway listener (outbound WS) so agents can *react*
rather than only post, and an MCP stdio adapter. Both run in the same local daemon.

Explicitly **not** in the plan: any deploy, any always-on host, scheduled posts, `accounts
export/import`, a second machine. degen-portal runs when you run it. Close the laptop and it
stops posting — that is the intended behaviour, and it deletes a daemon, a hosting bill, a
token-sync mechanism and a whole class of "why did my bot post at 4am" incidents.

## 10. Open decisions

1. **Token at rest.** 0600 JSON matches degen-tools, but a refresh token is permanent account
   control, strictly worse than a revocable API key. macOS Keychain via the `keyring` crate is one
   dependency and one `cfg`. Recommend Keychain for refresh tokens, file for everything else.
2. **X spend.** Pay-per-use since Feb 2026 ⇒ show estimated cost in the dashboard and make
   `max_per_day` default conservative (20).
3. **Name collision.** `degen-portal` vs `degen-tools` share `~/.degen-*`, ports 7717/7719, and
   two `connect` env var pairs. Keep them disjoint; do not teach one to read the other's store.
