//! X's chunked media upload: the one thing here that is not a single request.
//!
//! `initialize` opens a session, each chunk goes to `append`, `finalize`
//! closes it, and then the server may still be processing — a video is not
//! usable until `STATUS` says `succeeded`. Four calls with a session held
//! between them, which is why this is a [`degen_tools_core::NativeTool`]
//! rather than four tools an agent has to sequence while holding state.
//!
//! The agent sees one tool that returns a `media_id`, the same as the simple
//! image upload does.

use std::path::Path;
use std::time::{Duration, Instant};

use degen_tools_core::config::{CallContext, load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;
use serde_json::{Map, Value, json};

const API: &str = "https://api.x.com/2/media/upload";

/// X's own guidance: keep segments at or below 5 MB. 4 MB leaves headroom.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// X accepts up to 16 GB at initialize, but this reads the file into memory a
/// chunk at a time and holds the whole upload in one blocking call.
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Give up rather than poll forever on a stuck transcode.
const MAX_PROCESSING: Duration = Duration::from_secs(900);

pub const TOOL: degen_tools_core::NativeTool = degen_tools_core::NativeTool { id: "x_upload_video", run };

fn run(args: &Map<String, Value>, ctx: &CallContext<'_>) -> Result<Value, DegenError> {
    let path = args
        .get("media")
        .and_then(Value::as_str)
        .ok_or_else(|| DegenError::InvalidArgs("x_upload_video: --media is the path to the file".to_string()))?;
    let path = Path::new(path);
    let category = args.get("media_category").and_then(Value::as_str).unwrap_or("tweet_video");

    let bytes = std::fs::read(path).map_err(|e| DegenError::InvalidArgs(format!("x_upload_video: cannot read {}: {e}", path.display())))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(DegenError::InvalidArgs(format!(
            "x_upload_video: {} is {} bytes, over this tool's {MAX_BYTES} byte limit",
            path.display(),
            bytes.len()
        )));
    }
    if bytes.is_empty() {
        return Err(DegenError::InvalidArgs(format!("x_upload_video: {} is empty", path.display())));
    }
    let media_type = media_type_for(path)?;

    let base = base_url();
    let account = ctx.account;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(degen_tools_core::app().user_agent)
        .build()
        .map_err(|e| DegenError::Http(format!("failed to create HTTP client: {e}")))?;

    // 1. INIT
    let init = post_json(
        &client,
        account,
        &format!("{base}/initialize"),
        &json!({ "media_type": media_type, "total_bytes": bytes.len(), "media_category": category }),
    )?;
    let media_id = init["data"]["id"]
        .as_str()
        .ok_or_else(|| DegenError::Http(format!("initialize returned no media id: {init}")))?
        .to_string();

    // 2. APPEND, one segment at a time, indexed from zero.
    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        let part = reqwest::blocking::multipart::Part::bytes(chunk.to_vec())
            .file_name("chunk")
            .mime_str("application/octet-stream")
            .map_err(|e| DegenError::Http(format!("chunk {index}: {e}")))?;
        let form = reqwest::blocking::multipart::Form::new().text("segment_index", index.to_string()).part("media", part);
        let url = format!("{base}/{media_id}/append");
        let (authorization, _) = crate::xauth::authorize("POST", &url, account)?;
        let response = client
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .multipart(form)
            .send()
            .map_err(|e| DegenError::Http(format!("uploading chunk {index} failed: {}", e.without_url())))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            return Err(DegenError::Http(format!(
                "uploading chunk {index} of {} returned HTTP {status}: {body}",
                bytes.len().div_ceil(CHUNK_BYTES)
            )));
        }
    }

    // 3. FINALIZE
    let finalized = post_json(&client, account, &format!("{base}/{media_id}/finalize"), &Value::Null)?;

    // 4. STATUS, only when the server says it is still working.
    let state = finalized["data"]["processing_info"]["state"].as_str().unwrap_or("succeeded").to_string();
    let processing = match state.as_str() {
        "succeeded" | "" => state,
        _ => {
            let wait = finalized["data"]["processing_info"]["check_after_secs"].as_u64().unwrap_or(1);
            poll_until_done(&client, account, &base, &media_id, wait)?
        }
    };

    Ok(json!({
        "data": { "id": media_id, "media_type": media_type, "size": bytes.len(), "processing": processing },
        "next": "attach it with x_post --media_ids '[\"<id>\"]'",
    }))
}

/// Poll `STATUS` until X finishes transcoding, honouring the interval it asks
/// for. A `failed` state is an error: the media id would be useless.
fn poll_until_done(
    client: &reqwest::blocking::Client,
    account: Option<&str>,
    base: &str,
    media_id: &str,
    first_wait: u64,
) -> Result<String, DegenError> {
    let deadline = Instant::now() + MAX_PROCESSING;
    let mut wait = first_wait.clamp(1, 30);
    loop {
        std::thread::sleep(Duration::from_secs(wait));
        let url = format!("{base}?command=STATUS&media_id={media_id}");
        let (authorization, _) = crate::xauth::authorize("GET", &url, account)?;
        let response = client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .send()
            .map_err(|e| DegenError::Http(format!("checking upload status failed: {}", e.without_url())))?;
        let body: Value = serde_json::from_str(&response.text().unwrap_or_default())
            .map_err(|e| DegenError::Http(format!("status returned something unexpected: {e}")))?;
        let info = &body["data"]["processing_info"];
        match info["state"].as_str().unwrap_or("succeeded") {
            "succeeded" => return Ok("succeeded".to_string()),
            "failed" => {
                let why = info["error"]["message"].as_str().unwrap_or("no reason given");
                return Err(DegenError::Http(format!("X could not process the upload: {why}")));
            }
            _ => {
                if Instant::now() > deadline {
                    return Err(DegenError::Http(format!(
                        "the upload was accepted but X is still processing it after {} minutes. The media id is {media_id}; check it with x_upload_status before posting.",
                        MAX_PROCESSING.as_secs() / 60
                    )));
                }
                wait = info["check_after_secs"].as_u64().unwrap_or(wait).clamp(1, 30);
            }
        }
    }
}

fn post_json(client: &reqwest::blocking::Client, account: Option<&str>, url: &str, body: &Value) -> Result<Value, DegenError> {
    let (authorization, _) = crate::xauth::authorize("POST", url, account)?;
    let mut request = client.post(url).header(reqwest::header::AUTHORIZATION, authorization);
    if !body.is_null() {
        request = request.json(body);
    }
    let response = request.send().map_err(|e| DegenError::Http(format!("{url} failed: {}", e.without_url())))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(DegenError::Http(format!("{url} returned HTTP {}: {text}", status.as_u16())));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| DegenError::Http(format!("{url} returned something unexpected: {e}")))
}
/// X needs the real type: it decides the transcode path from it.
fn media_type_for(path: &Path) -> Result<&'static str, DegenError> {
    match path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default().as_str() {
        "mp4" | "m4v" => Ok("video/mp4"),
        "mov" => Ok("video/quicktime"),
        "webm" => Ok("video/webm"),
        "gif" => Ok("image/gif"),
        other => Err(DegenError::InvalidArgs(format!(
            "x_upload_video: '{other}' is not a format X takes here. Use mp4, mov, webm or gif; \
             for a still image use x_upload_media."
        ))),
    }
}

/// Tests point this at a local server. Only loopback is honoured: this URL
/// receives the account's access token, and a `.env` that could redirect it
/// anywhere would be a way to hand the token to a stranger.
fn base_url() -> String {
    match lookup_credential(&load_credentials().unwrap_or_default(), "X_MEDIA_BASE_URL") {
        Some(url) if url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:") => url,
        Some(url) => {
            eprintln!("warning: ignoring X_MEDIA_BASE_URL={url} — only a loopback address may replace {API}");
            API.to_string()
        }
        None => API.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_formats_x_can_transcode_are_accepted() {
        assert_eq!(media_type_for(Path::new("a.mp4")).unwrap(), "video/mp4");
        assert_eq!(media_type_for(Path::new("A.MOV")).unwrap(), "video/quicktime");
        let err = media_type_for(Path::new("a.png")).unwrap_err().to_string();
        assert!(err.contains("x_upload_media"), "a still image has its own tool: {err}");
        assert!(media_type_for(Path::new("noext")).is_err());
    }

    #[test]
    fn a_file_is_cut_into_segments_x_will_accept() {
        // The chunking is plain slicing, but the size is the thing X rejects on.
        assert!(CHUNK_BYTES <= 5 * 1024 * 1024, "X caps a segment at 5 MB");
        let bytes = vec![0u8; CHUNK_BYTES * 2 + 17];
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_BYTES).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].len(), 17, "the last segment is the remainder");
    }
}
