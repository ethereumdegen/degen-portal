---
description: Post, edit, delete and read Discord messages as a bot or through a channel webhook, with a human-held allowlist deciding which channels can be written to
version: 0.1.0
---

# Discord

**A message you send is public and permanent until you delete it.** If a call
fails, read the error and stop. Do not call it again: a 429 or a timeout can
mean the message was already posted, and a retry posts it twice.

Two ways in, and you may have either or both:

- **Bot** (`DISCORD_BOT_TOKEN`) — everything here. `Authorization: Bot <token>`.
- **Webhook** (`DISCORD_WEBHOOK_URL`) — `discord_send_webhook` only. No bot, no
  permissions, one channel per URL. The fastest way to get one channel working.

## Setup (the human does this once)

```bash
# 1. https://discord.com/developers/applications -> New Application -> Bot -> Reset Token
degen-portal auth set DISCORD_BOT_TOKEN          # paste on stdin; it is never printed again
degen-portal auth set DISCORD_CLIENT_ID <id>     # the Application ID, not a secret

# 2. Add it to the server. No callback URL, no redirect, nothing hosted.
degen-portal discord invite                      # prints the URL; open it, pick the server

# 3. Say which channels the bot may write to
degen-portal run discord_list_channels --guild_id <server id>
degen-portal discord allow <channel id>
```

Leave **Public Bot** unchecked in the application's Bot settings so nobody else
can add it anywhere.

## The allowlist

Every write that names a `channel_id` — send, edit, delete, react, thread — is
refused unless that exact channel was allowed on this machine. Reads are never
refused, so `discord_list_channels` and `discord_get_messages` work everywhere
the bot can see.

This is deliberate: an agent that can list channels can find `#announcements`,
and "the agent had the id" is not permission to post there. If a call is
refused, the error names the command a human runs to allow it. Ask; do not work
around it.

## Posting

```bash
degen-portal run discord_send_message --channel_id 1180000000000000000 --content "gm"
#  -> { "id": "1190...", "channel_id": "1180...", ... }   the id is the handle for edit/delete/react
```

- `content` is Markdown, up to 2000 characters. Longer must be split by you.
- `--reply_to <message id>` makes it a reply.
- **Mentions never notify.** `@everyone`, `@here` and role pings render as plain
  text. There is no parameter to change that.
- Forum channels (type 15) need `--thread_name`; a plain `content` post fails.

Undo is `discord_delete_message --channel_id C --message_id M`. It returns an
empty body and HTTP 204; that is success. `degen-portal undo` deletes the most
recent message without you needing the ids.

Two limits apply before the API is called: the same message to the same channel
inside 15 minutes is refused as a repeat, and 30 messages an hour (200 a day)
is the default cap. Both refuse without sending anything.

## Reading a conversation

```bash
degen-portal run discord_get_messages --channel_id C --limit 20
degen-portal run discord_get_messages --channel_id C --after <last id you saw>   # poll for replies
```

`content` comes back empty for messages the bot did not write unless the
application has the **Message Content** privileged intent enabled. Enable it in
the Developer Portal under Bot -> Privileged Gateway Intents when an agent needs
to read what other people said.

There is no live connection: this polls. Nothing arrives while nothing asks.

## Tools

| Tool | |
|---|---|
| `discord_me` | Who the token belongs to. Start here when something is wrong. |
| `discord_list_guilds` | Servers the bot is in. Empty means it was never invited. |
| `discord_list_channels` | Channels in a server, with ids. |
| `discord_get_channel` | One channel's name, topic and type. |
| `discord_get_messages` | Recent messages, newest first. |
| `discord_send_message` | Post. Public and permanent. |
| `discord_edit_message` | Edit one of the bot's own messages. |
| `discord_delete_message` | Delete. The undo for a bad post. |
| `discord_add_reaction` | React as the bot. |
| `discord_create_thread` | Thread off a message. The new thread is itself a channel, and needs allowing before the bot can write in it. |
| `discord_send_webhook` | Post through a webhook URL, no bot involved. |

## When something fails

| What you see | What it means |
|---|---|
| `refuses channel <id>: it is not on this machine's allowlist` | Working as intended. Ask the human to run `degen-portal discord allow <id>`. |
| HTTP 401 `401: Unauthorized` | The bot token is wrong or was reset. |
| HTTP 403 `Missing Access` | The bot is not in that server, or cannot see that channel. |
| HTTP 403 `Missing Permissions` | It can see the channel but may not post; fix the role in Discord. |
| HTTP 404 `Unknown Channel` | Wrong id — that is a message id, or a channel in another server. |
| HTTP 429 | Rate limited. **Do not retry.** Report it; the body says how long. |
