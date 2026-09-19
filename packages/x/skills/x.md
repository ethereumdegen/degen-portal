---
description: Post, reply, quote, poll, search, read mentions, like and repost on X as a connected account, with tokens refreshed automatically and never shown
version: 0.1.0
---

# X

**Everything here is public, permanent and billed.** A post is visible to
everyone, stays until deleted, and costs money: since February 2026 X has no
free tier for new developers — it is pay-per-use, roughly a cent per post.

If a call fails, **read the error and stop**. A 429 or a timeout can mean the
post landed anyway; calling again is how the same thought gets published twice
and how a rate limit becomes a bill.

## Setup (the human does this once)

```bash
# 1. https://developer.x.com -> an app with OAuth 2.0 enabled, type "Native App"
#    Callback URL:  http://localhost:7720/oauth/callback     (exactly this)
degen-portal auth set X_CLIENT_ID <client id>    # not a secret; a public client needs no secret

# 2. Connect the account. Opens a browser once.
degen-portal connect x
```

If `/2/*` calls come back with `client-forbidden` or `client-not-enrolled`
*after* auth worked, the app is not enrolled: in the developer console, move it
to the **Pay-per-use** package and the **Production** environment. That is an X
platform setting, not anything wrong here.

## Accounts

The access token lasts two hours and is refreshed automatically, under a lock,
before it is used. You never see it and never need to.

```bash
degen-portal accounts                    # who is connected, and when each token expires
degen-portal accounts default x:handle   # which one acts by default
degen-portal run --account x:other x_post --text "..."
```

With one account connected, it is used. With several and no default, a call is
refused rather than guessed — posting as the wrong handle cannot be undone by
deleting it.

## Posting

```bash
degen-portal run x_post --text "gm"
#  -> { "data": { "id": "1790...", "text": "gm" } }

degen-portal run x_post --text "and another thing" --reply_to 1790...
degen-portal run x_post --text "look at this" --quote_id 1790...
degen-portal run x_post --text "pick one" --poll_options '["a","b"]' --poll_minutes 1440
```

- 280 characters, unless the account is on a Pro tier. Links count as 23
  characters however long they are.
- A thread is `x_post`, then `x_post --reply_to <the id it returned>`, one call
  per post. There is no thread tool: each post is separately public and
  separately billed.
- `--reply_settings following` or `mentionedUsers` limits who can reply.

Undo is `x_delete_post --id <id>`. It works only on the account's own posts.

## Reading

```bash
degen-portal run x_me                                    # the acting account's id and handle
degen-portal run x_list_mentions --id <user id> --since_id <last seen>
degen-portal run x_search_recent --query "from:someone -is:retweet"
degen-portal run x_list_posts --id <user id> --exclude retweets,replies
```

`x_search_recent` covers the last 7 days only. Reads are billed too, so poll
with `--since_id` rather than re-reading the same timeline.

## Tools

| Tool | |
|---|---|
| `x_me` | Which account this acts as. |
| `x_post` | Publish. Public, permanent, billed. |
| `x_delete_post` | Delete one of the account's own posts. |
| `x_get_post` | One post with author and metrics. |
| `x_search_recent` | Search the last 7 days. |
| `x_list_mentions` | What mentions the account, for replying. |
| `x_list_posts` | What the account already said. |
| `x_like` / `x_unlike` | Like, visibly. |
| `x_repost` / `x_unrepost` | Put someone else's words on the timeline. Publishing, in effect. |

Media upload is not here yet: it needs chunked uploads, which land with the
`multipart` body mapping.

## When something fails

| What you see | What it means |
|---|---|
| `no x account is connected` | Ask the human to run `degen-portal connect x`. |
| `N x accounts are connected and none is the default` | Ask which handle to post as. Do not pick. |
| `refreshing the access token failed` | The refresh token was revoked or rotated away. The human reconnects. |
| HTTP 401 | The token was revoked while in flight. Reconnect. |
| HTTP 403 with `client-forbidden` | App enrollment, not code. See Setup. |
| HTTP 403 `duplicate content` | X refuses identical posts. You already said this. |
| HTTP 429 | Rate limited or out of credits. **Do not retry.** Report it. |
