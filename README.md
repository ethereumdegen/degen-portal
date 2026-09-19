# degen-portal

[degen-tools](https://github.com/ethereumdegen/degen-tools) for social. The same
idea — one HTTP call per tool, credentials injected and never printed, an agent
that can run a shell command can use it — pointed at **Discord** and **X**.

The difference is that these calls are public, permanent and billed, so there is
a layer between an agent and the send button.

```bash
cargo install --path .

degen-portal                     # local API for agents on 127.0.0.1:7719
degen-portal list                # packages, credentials, accounts, budgets
degen-portal skill x             # the guide an agent reads first
degen-portal run x_post --text "gm"
```

No backend, no deploy, no hosted anything. It runs when you run it, on the
machine in front of you.

## Setup

### Discord (five minutes, no OAuth)

```bash
# https://discord.com/developers/applications -> New Application -> Bot -> Reset Token
degen-portal auth set DISCORD_BOT_TOKEN        # read from stdin, never printed
degen-portal auth set DISCORD_CLIENT_ID <id>   # the Application ID; not a secret

degen-portal discord invite                    # prints the URL; open it, pick the server
degen-portal run discord_list_channels --guild_id <server id>
degen-portal discord allow <channel id>        # now the bot may write there
```

Adding a bot needs no callback URL and nothing hosted — Discord's bot flow is
"server-less and callback-less" by design. Leave **Public Bot** unchecked so
only you can add it anywhere.

A channel webhook works too, with no bot at all: store the URL as
`DISCORD_WEBHOOK_URL` and use `discord_send_webhook`.

### X (one browser trip)

```bash
# https://developer.x.com -> an app with OAuth 2.0, type "Native App"
#   Callback URL:  http://localhost:7720/oauth/callback
degen-portal auth set X_CLIENT_ID <client id>  # a public client needs no secret
degen-portal connect x                         # opens a browser once
```

PKCE, so the `code_verifier` never leaves this machine. The access token lasts
two hours and is refreshed automatically, under a lock file, because X rotates
refresh tokens and two processes racing that rotation would strand the account.

If `/2/*` calls return `client-forbidden` after auth worked, move the app to the
**Pay-per-use** package and **Production** environment in X's console.

## What refuses you, and why

Every gate refuses *before* anything is sent, so a refusal means nothing
happened.

| Gate | |
|---|---|
| **Channel allowlist** | A Discord write needs the channel allowed by a human. An agent that can list channels can find `#announcements`; having the id is not permission. |
| **Repeat window** | The same publish twice inside 15 minutes is refused, naming the post it would have duplicated. This is what catches a retry after a timeout. |
| **Budgets** | X 10/hour and 20/day, Discord 30/hour and 200/day. The refusal says when the next slot frees. `degen-portal budget x --per-day 50` to change. |
| **Approval queue** | Opt in per provider with `degen-portal approval x queue`: calls are held, and a human runs `approve` or `drop`. |

Nothing is ever retried automatically. Mentions never notify: `@everyone` and
role pings are rendered as text, and no parameter turns that back on.

Everything published is written to `~/.degen-portal/ledger.jsonl` with the call
that undoes it:

```bash
degen-portal log            # what went out, and how much budget is left
degen-portal undo           # delete the most recent post
```

## Agents

Three ways in, all through the same engine and the same gates.

**Shell:** `degen-portal run <tool> --param value`

**Local API:**

```bash
eval "$(degen-portal connect)"     # DEGEN_PORTAL_URL, DEGEN_PORTAL_TOKEN
curl -s $DEGEN_PORTAL_URL/v1/run -H "Authorization: Bearer $DEGEN_PORTAL_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"tool":"x_post","args":{"text":"gm"}}'
```

127.0.0.1 only, a fresh token per run, and requests carrying a browser `Origin`
or a foreign `Host` are refused, so a web page cannot reach it.

**MCP:** `degen-portal mcp` speaks the Model Context Protocol on stdin/stdout.

```json
{
  "mcpServers": {
    "degen-portal": { "command": "degen-portal", "args": ["mcp"] }
  }
}
```

It exposes every tool plus `portal_status` — connected accounts, writable
channels, budget left — so an agent can ask what it may do before doing it. A
refusal comes back as tool content the model reads, not a transport error it
never sees.

## Packages

| Package | Credentials | Tools |
|---|---|---|
| `discord` | `DISCORD_BOT_TOKEN`, `DISCORD_WEBHOOK_URL`, `DISCORD_CLIENT_ID` | Send, edit, delete, react, threads, attachments, read messages, list servers and channels, webhook posting. |
| `x` | connected account (OAuth) | Post, reply, quote, poll, delete, image upload, search, mentions, own timeline, like, repost. |

Same format as degen-tools: `integration.json` plus one JSON file per tool, in
the metalcraft integration format, so a package runs here and in the agent.

## Commands

| Command | |
|---|---|
| `degen-portal` / `serve` | Local API on 127.0.0.1:7719 |
| `connect` | Print `DEGEN_PORTAL_URL` / `DEGEN_PORTAL_TOKEN` for agents |
| `connect x [--headless]` | Connect an X account over OAuth |
| `accounts` / `accounts default` / `accounts revoke` | Connected accounts and token expiry |
| `discord invite \| allow \| deny \| channels` | Bot install URL and the channel allowlist |
| `run <tool> --param value [--account x:handle]` | Call a tool |
| `skill [package\|tool]` | Agent-facing docs |
| `log` / `undo` | What was published, and take it back |
| `queue` / `approve` / `drop` | Calls held for a human |
| `budget <provider>` / `approval <provider> auto\|queue` | Change the limits |
| `mcp` | Serve the tools over MCP on stdin/stdout |
| `auth set\|get\|list\|remove` | Stored credentials |
| `list` | Packages, credentials, accounts, budgets |

## Storage

`~/.degen-portal/`, all owner-only (`0600`):

| File | |
|---|---|
| `credentials.json` | Bot tokens and webhook URLs, unless they live in the project `.env` |
| `accounts.json` | OAuth access and refresh tokens, written atomically |
| `policy.json` | Channel allowlist, budgets, approval mode |
| `ledger.jsonl` | Everything published, with its undo |
| `queue.json` | Calls waiting for approval |

Credentials are per project first: the `.env` in the current directory (or the
nearest one up to the git root) wins over the global store, so a side project
can post as a different bot without touching anything else.

A refresh token is permanent control of an account — worse than a revocable API
key. It is stored 0600 like everything else; putting it in the macOS Keychain is
the obvious next step.

## Not here

- **X video.** It needs four calls (initialize, append per chunk, finalize, poll
  status) and every tool here is one HTTP request. Images work.
- **Anything hosted.** No relay, no broker, no always-on daemon. Close the
  laptop and nothing posts, which is the point.
- **Reacting in real time.** Reading is polling (`discord_get_messages
  --after`). A gateway listener would be an outbound WebSocket, and is not
  written yet.

## Built on

`degen-core`, the engine extracted from degen-tools: package format, credential
resolution, secret masking, the host allowlist and the loopback API. Both
binaries share it so the security-critical parts exist once.

## License

MIT
