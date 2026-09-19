---
skill: degen-portal
version: {version}
description: Post and read on X and in Discord; X authenticates per account over OAuth with tokens refreshed automatically, and a human-held allowlist decides which Discord channels can be written to
---

# degen-portal

degen-portal posts for you. Every tool is one HTTP request: it fills the request
from your `--name value` arguments, adds the token the service needs, sends it,
and prints the JSON response. Tokens are stored once by the user and are never
shown to you.

**What makes this different from a normal API client: the calls are public and
permanent.** A message you send can be read by everyone in the channel and stays
there until it is deleted. Act accordingly:

- **Never repeat a failed write.** A 429, a timeout or a broken connection can
  mean the post landed anyway. Report the error; do not call the tool again.
- **One post per intention.** If you meant to say one thing, call the tool once.
- **Write like it is signed by the user, because it is.**

## Packages

{packages}
## Commands

```bash
degen-portal list                             # packages, credentials, connected accounts
degen-portal accounts                         # X accounts and when each token expires
degen-portal log                              # what was published, and how much budget is left
degen-portal status                           # accounts, channels, budget left — check before a burst
degen-portal skill <package>                  # a package's guide and every tool's parameters
degen-portal skill <tool>                     # one tool's parameters
degen-portal run <tool> --param value ...     # call a tool
degen-portal run --json '{...}' <tool>        # arguments as JSON (or --json @file.json)
degen-portal discord channels                 # which channels may be written to
```

## What will refuse you, and why

Four gates sit in front of every write. All of them refuse *before* anything is
sent, so a refusal means nothing happened — it is not a partial post.

1. **The channel allowlist** (Discord). Ask a human; do not look for a way
   around it.
2. **Repeats.** Publishing exactly the same thing twice inside 15 minutes is
   refused, and the refusal names the post you already made. This is what
   catches a retry after a timeout. **If you get it, you already succeeded.**
3. **Budgets.** Each provider has an hourly and a daily cap on published calls.
   The refusal says when the next slot frees. Do not sit in a loop waiting for
   it; report it and stop.
4. **Approval.** A provider can be set to hold calls for a human. You get
   `{"queued": "q1"}` and the call is not sent. Tell the user it is waiting.

`degen-portal log` shows what has been published and how much of each budget is
left. Read it before a burst of posts, not after.

## Accounts and the allowlist

Two different gates, one per provider.

**X** acts as a connected account. With one connected it is used; with several
and no default, a call is refused rather than guessed — ask which handle. The
access token is refreshed automatically and never shown to you. Add
`--account x:handle` to pick one.

**Discord**: writing to a channel is refused unless a human has allowed that exact
channel id on this machine. Reads are never refused, so you can list channels
and read messages to work out which id to ask about.

If a call is refused, the error names the command the human runs:

```
degen-portal discord allow <channel id>
```

Ask them to run it. There is no way around it from here, and looking for one is
the wrong instinct: the allowlist is what makes it safe to give an agent a
posting tool at all.

## Where credentials come from

Per project: the `.env` in the current directory (or the nearest one up to the
git root) wins, then the global store, then the shell environment. If a token is
missing, ask the user to run `degen-portal auth set <NAME>` in their own
terminal — it reads the value from stdin, so it never lands in chat, in shell
history, or in your transcript.

## The local API (when the user runs `degen-portal serve`)

```bash
eval "$(degen-portal connect)"   # DEGEN_PORTAL_URL, DEGEN_PORTAL_TOKEN
curl -s $DEGEN_PORTAL_URL/v1/status -H "Authorization: Bearer $DEGEN_PORTAL_TOKEN"
curl -s $DEGEN_PORTAL_URL/v1/run -H "Authorization: Bearer $DEGEN_PORTAL_TOKEN" -H 'Content-Type: application/json' \
  -d '{"tool": "discord_send_message", "args": {"channel_id": "...", "content": "gm"}}'
```

`POST /v1/run` takes `{tool, args, secrets: {param: CREDENTIAL}, cred: {VAR: CREDENTIAL}}`
and returns `{ok, status, error, response, duration_ms}`.
`GET /v1/packages/{id}` returns a package's guide and every tool's schema.

## Files

Both providers take a local path: `x_upload_media --media ./hero.png` gives an
id for `x_post --media_ids`, and `discord_upload_attachment --file ./chart.png`
sends the file and the message in one call. `x_upload_video --media ./clip.mp4`
uploads a video in chunks and waits for X to transcode it, which can take
minutes — call it once. Whatever path you name is read off this machine and
published, so name only what the user asked to publish.

## Reading results

- Exit code 0: success; the response body is on stdout. For a post, the `id` in
  that body is the handle for editing or deleting it.
- Non-zero exit: the error is on stderr, and the API's own response body is still
  on stdout. Read it before doing anything else.
- `run` refuses unknown or missing parameters and says which.

## Rules for AI agents

- Read `degen-portal skill <package>` before using a package for the first time.
- Never print a `.env` or a credentials file, and never ask for a token in chat.
- A write that fails is finished. Report it and stop.
- Read before you write: list the channels, read the recent messages, then post.
