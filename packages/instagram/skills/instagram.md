---
description: Publish images, reels, stories and carousels to an Instagram professional account and answer direct messages, with a human-held allowlist deciding who can be messaged and no way to delete a post
version: 0.1.0
---

# Instagram

**A post is public, permanent, and — unlike X and Discord — there is no delete
in the API.** Instagram's API can publish a post and cannot remove one.
`degen-portal undo` cannot take it back. Nothing here can. Only a human, in the
Instagram app, can delete a post an agent published.

So if a call fails, **read the error and stop**. A 429 or a timeout can mean
the post landed anyway, and a retry publishes it twice with no way to remove
either copy. A failed write is finished. Check `degen-portal log` or
`instagram_list_media` before concluding that nothing happened.

## Setup (the human does this once)

```bash
# 1. https://developers.facebook.com -> an app with the Instagram product added,
#    "Instagram API with Instagram Login". No Facebook Page is involved.
degen-portal auth set INSTAGRAM_APP_ID                  # paste on stdin
degen-portal auth set INSTAGRAM_APP_SECRET              # paste on stdin; never printed again
degen-portal auth set INSTAGRAM_REDIRECT_URI <https URI registered in the app dashboard>

# 2. Connect the account. Opens a browser once.
degen-portal connect instagram
```

The account has to be an Instagram **professional** account — business or
creator. A personal account can authorize the app and then fail every call
here; `instagram_me` shows `account_type`, and it must be `BUSINESS` or
`MEDIA_CREATOR`.

The access token is long-lived, refreshed automatically before it is used, and
never shown. You never type it and never need to.

## Media comes from a public URL, not from disk

Instagram does not accept an upload. You give it an HTTPS URL and **Instagram's
servers fetch the file themselves**, so the URL has to be reachable from the
public internet: `file://`, `localhost` and a path on this machine all fail.
`x_upload_media` and `discord_upload_attachment` take a local path; nothing in
this package does. Put the file somewhere public first, then pass the URL.

**Images must be JPEG.** PNG, WEBP and HEIC are refused, usually as a bare HTTP
400 on `image_url`. Video is MP4 or MOV.

## Publishing is two calls

```bash
degen-portal run instagram_create_media --image_url https://cdn.example/hero.jpg --caption "gm"
#  -> { "id": "17999..." }        a container. Nothing is public yet.

degen-portal run instagram_publish_media --creation_id 17999...
#  -> { "id": "18000..." }        this one is the post, and it is now public forever

degen-portal run instagram_get_media --media_id 18000...   # -> permalink a human can open
```

- `instagram_create_media` is not publishing: it stages a container that
  expires in 24 hours. Only `instagram_publish_media` puts something in the
  world, and only that call is counted against the budget and written to the
  ledger.
- Pass **either** `--image_url` **or** `--video_url`, never both.
- Captions: 2200 characters, 30 hashtags.
- Video, reels and stories are processed asynchronously. Poll
  `instagram_media_status --container_id <id>` until `status_code` is
  `FINISHED`, then publish. `IN_PROGRESS` is not a failure; `ERROR` and
  `EXPIRED` mean build a new container, not try the publish again.
- Reels: `--media_type REELS --video_url ...`. Stories:
  `--media_type STORIES`, and they vanish after 24 hours on their own.
- Carousel: one container per slide with `--is_carousel_item true`, then a
  parent with `--media_type CAROUSEL --children 17999...,17998...` (the ids,
  comma-separated) and the caption, then publish the parent. Up to 10 slides;
  the whole carousel counts as one post.
- Set `--is_ai_generated true` when the media was generated. Instagram labels
  it; not saying so is a lie told in the account's name.

**100 published posts per 24 hours** per account, counted by Instagram. A
carousel is one.

## Direct messages

A DM here is a reply, never an opening. Finding who to reply to:

```bash
degen-portal run instagram_list_conversations                       # who has written
degen-portal run instagram_get_conversation --conversation_id 1...  # message ids and times
degen-portal run instagram_get_message --message_id 1...            # the text, and from.id
degen-portal run instagram_send_dm --recipient_id <from.id> --text "on it"
```

`from.id` is the Instagram-scoped id (IGSID) — an id for that person *in this
account's inbox only*, not a username and not portable to another app. It is
the only thing `--recipient_id` accepts.

Three rules, and all three refuse rather than bend:

1. **They must message first.** There is no way to open a conversation with
   someone who has not written to the account. Anyone absent from
   `instagram_list_conversations` cannot be messaged at all.
2. **24 hours from their last message.** After that the API answers error code
   10, subcode 2534022. That is the rule, not a bug and not a transient error.
   Do not retry it; say the window closed.
3. **The recipient must be allowed on this machine.** A human runs
   `degen-portal instagram allow <igsid>` for each person. Being able to read
   an inbox is not permission to answer it.

Cold outreach — DMing people who did not write first, or scraping ids to pitch
them — is against Meta's platform rules and gets accounts banned. Asked to do
it, say no and say why. Do not go looking for a way around the window or the
allowlist.

Only the 20 most recent messages in a conversation can be read in detail.
A sent DM cannot be unsent through the API.

Images by DM: `instagram_send_dm_image --recipient_id <igsid> --image_url
https://cdn.example/x.png` — public URL again, PNG or JPEG, 8 MB.

## The gates that refuse before anything is sent

- **Recipient allowlist.** Every DM to an IGSID a human has not allowed is
  refused before the API is called. The error names the command to run.
- **Repeat window.** The same post or the same message to the same recipient
  inside 15 minutes is refused as a repeat, naming what was already sent.
- **Budget.** An hourly and daily cap on publishes and DMs. The refusal says
  when a slot frees. Report it; do not spin.
- **Approval queue.** Where a human asked to see writes first, the call is
  parked until they approve it. Parked is not failed.

Reads are never gated. `instagram_me`, `instagram_list_media`,
`instagram_get_media`, `instagram_media_status` and the three conversation
tools always work.

## Tools

| Tool | |
|---|---|
| `instagram_me` | Which account this acts as, and whether it is a professional account. Start here. |
| `instagram_list_media` | What the account already posted, newest first. |
| `instagram_get_media` | One post in full — turns a published id into a permalink. |
| `instagram_media_status` | Whether a container is `FINISHED` and safe to publish. |
| `instagram_create_media` | Stage a container from a public URL. Not public yet. |
| `instagram_publish_media` | Publish. Public, permanent, no delete. |
| `instagram_list_conversations` | Who has written to the account. |
| `instagram_get_conversation` | Message ids and times in one conversation. |
| `instagram_get_message` | One message: the text, and the IGSID to reply to. |
| `instagram_send_dm` | Reply to someone who messaged first. Cannot be unsent. |
| `instagram_send_dm_image` | The same, with an image from a public URL. |

## When something fails

| What you see | What it means |
|---|---|
| `refuses recipient <igsid>: they are not on this machine's allowlist` | Working as intended. Ask the human to run `degen-portal instagram allow <igsid>`. |
| error code 10, subcode 2534022 | Outside the 24-hour window since their last message. Not retryable; the conversation is closed until they write again. |
| `OAuthException` code 190 | The token expired or was revoked. The human runs `degen-portal connect instagram` again. |
| `(#100) Media ID is not available` / `The media is not ready` | The container is still processing. Poll `instagram_media_status`; publish when `status_code` is `FINISHED`. |
| HTTP 400 on `image_url` | Instagram could not fetch it, or it is not a JPEG. The URL must be publicly reachable over HTTPS. |
| `(#4) Application request limit reached` | Rate limited. **Do not retry.** Report it. |
| `Only owners of the URL can publish` / personal account errors | The account is not a professional account, or the app is not the connected one. See Setup. |
