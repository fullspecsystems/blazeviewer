//! AI image-description backends (task #44, subtask 4).
//!
//! One seam, [`Describer`] ("prepared image + prompt → text"), with the pure-Rust
//! [`LocalEndpoint`] backend that POSTs to an **OpenAI-compatible**
//! `/v1/chat/completions` (LM Studio / llama.cpp server / Ollama). It runs the blocking
//! [`ureq`] call on a **worker thread** ([`describe_job`], the `image_text::scan_job`
//! shape) — never `tokio`, never the event loop. The macOS Apple Foundation Models
//! backend is *not* here: it's delegated to the Swift host via a `CoreEffect`, so it
//! never crosses this trait.
//!
//! Privacy (task #2 / ADR-018): the endpoint receives the image only on an explicit
//! describe command (or opt-in dwell), and only ever the URL the user typed. The image
//! is downscaled + JPEG-re-encoded first ([`prepare_image`]) — the original file bytes
//! (HEIC/RAW) never leave, and most servers wouldn't accept them anyway. Nothing is
//! written to disk.
//!
//! **Testability:** the request building, response parsing, URL joining, and error
//! mapping are pure functions with unit tests; only the socket call itself isn't
//! exercised in-process (that's the dev example / live smoke).

use std::sync::mpsc::Receiver;

use base64::Engine as _;
use pb_render::Rotation;
use pb_source::ItemSource;
use serde_json::{json, Value};

use crate::engine::{rotate_rgba8, to_clipboard_rgba8};

/// An in-flight off-thread describe for `item` — the [`image_text::TextScan`] shape: kicked
/// by `D` / Ask, polled by the tick, superseded by dropping the receiver. `gen` is the deck
/// generation at kick time, so a late result after a playlist rebuild is discarded.
///
/// [`image_text::TextScan`]: crate::image_text::TextScan
pub struct DescribeScan {
    pub gen: u64,
    pub item: usize,
    /// The frame being described (task #81).
    pub frame: crate::frame_capture::FrameKey,
    /// Copy the description to the clipboard when it lands (the Copy command ran while the
    /// describe was still in flight) — the `image_text::TextScan::copy_when_done` shape.
    pub copy_when_done: bool,
    pub rx: Receiver<Result<String, DescribeError>>,
}

/// Longest edge (px) the endpoint image is downscaled to before encoding — enough for a
/// vision model to read a scene without shipping a 24 MP frame over the wire.
pub const MAX_EDGE: u32 = 1024;
/// JPEG quality for the endpoint image (visually clean, small on the wire).
pub const JPEG_QUALITY: u8 = 85;

/// Connect timeout — short, so a dead endpoint fails fast with a clear message rather
/// than hanging the "Describing…" state.
const CONNECT_TIMEOUT_SECS: u64 = 4;
/// Overall request timeout — generous, since a large local model can take several
/// seconds to a couple of minutes on the first token.
const GLOBAL_TIMEOUT_SECS: u64 = 120;
/// Cap the reply length — a 2–4 sentence description needs little, and an unbounded
/// generation on a chatty model would stall the panel.
const MAX_TOKENS: u32 = 512;
/// Extra `max_tokens` on top of the length preset for a model that **thinks**. LM Studio
/// (like the OpenAI API) counts the hidden `reasoning_content` against `max_tokens`, so a
/// thinking model given only the preset can spend all of it thinking and answer with
/// nothing. Measured 2026-11-14 on `qwen3.6-35b-a3b`: 820 reasoning tokens + a 132-token
/// answer for the default describe prompt on a 1024 px image (fails at Standard's 512),
/// 268 of a 300 cap with no answer at all, and 44 for a "what text is here" Ask. See
/// [`run_adaptive`].
const REASONING_HEADROOM: u32 = 4096;

/// A describe attempt's failure, mapped to a one-line, user-facing message by
/// [`DescribeError::user_message`]. Kept coarse on purpose — the panel shows one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DescribeError {
    /// No usable backend (Auto found neither Apple FM nor a configured endpoint).
    NotConfigured,
    /// Couldn't open a connection — refused / DNS / no route. Almost always "the server
    /// isn't running".
    Unreachable,
    /// The request exceeded [`GLOBAL_TIMEOUT_SECS`].
    Timeout,
    /// The endpoint answered with a non-2xx HTTP status, plus the server's own error
    /// message when its body carried one (LM Studio's say what's wrong, e.g. a text-only
    /// model handed an image).
    BadStatus(u16, Option<String>),
    /// A 2xx reply carrying the server's own `error` object.
    ServerError(String),
    /// The model was still thinking when it hit the length cap, so there is no answer —
    /// even after the retry with [`REASONING_HEADROOM`].
    ThoughtTooLong,
    /// A well-formed reply with no description text in it.
    EmptyReply,
    /// The reply wasn't an OpenAI-style chat completion at all (not JSON, or no
    /// `choices[0].message.content`) — usually the URL points at the wrong thing.
    BadResponse(String),
    /// Preparing the image (decode / downscale / encode) failed.
    Prep(String),
    /// Any other backend/transport error.
    Backend(String),
}

impl DescribeError {
    /// The one-line message shown in the panel / toast.
    pub fn user_message(&self) -> String {
        match self {
            DescribeError::NotConfigured => {
                "No description backend is set up (Settings ▸ AI Descriptions).".to_string()
            }
            DescribeError::Unreachable => {
                // On macOS 15+ the usual cause of a blocked LAN endpoint is the app lacking
                // Local Network permission; elsewhere it's the server being off / a bad URL.
                #[cfg(target_os = "macos")]
                {
                    format!(
                        "Can't reach the model server for AI descriptions. Check that {} \
                         has Local Network permission (System Settings • Privacy & Security • \
                         Local Network).",
                        crate::APP_NAME
                    )
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "Can't reach the model server for AI descriptions. Check that the server \
                     (LM Studio / Ollama) is running and the endpoint URL is correct."
                        .to_string()
                }
            }
            DescribeError::Timeout => "The description timed out.".to_string(),
            DescribeError::BadStatus(code, Some(msg)) => {
                format!("The model server returned an error (HTTP {code}): {msg}")
            }
            DescribeError::BadStatus(404, None) => "The model server returned HTTP 404 — \
                check the endpoint URL (OpenAI-compatible servers usually end in /v1)."
                .to_string(),
            DescribeError::BadStatus(code, None) => {
                format!("The model server returned an error (HTTP {code}).")
            }
            DescribeError::ServerError(msg) => format!("The model server reported an error: {msg}"),
            DescribeError::ThoughtTooLong => "The model was still thinking when it hit the \
                length limit, so there's no description. Try a longer response length, or a \
                model (or model setting) that thinks less."
                .to_string(),
            DescribeError::EmptyReply => "The model returned an empty description.".to_string(),
            DescribeError::BadResponse(why) => format!(
                "The server's reply isn't an OpenAI-style chat completion ({why}). Check that \
                 the endpoint URL points at an OpenAI-compatible API (usually ending in /v1)."
            ),
            DescribeError::Prep(why) => format!("Couldn't prepare the image ({why})."),
            DescribeError::Backend(why) => why.clone(),
        }
    }
}

impl std::fmt::Display for DescribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for DescribeError {}

/// The image handed to a [`Describer`]: downscaled, JPEG-encoded, opaque sRGB8 bytes.
/// Held as JPEG so the endpoint path base64s it into a data URI; a hardware backend that
/// wants raw pixels would carry a different payload behind the same seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedImage {
    pub jpeg: Vec<u8>,
}

impl PreparedImage {
    /// `data:image/jpeg;base64,…` for an OpenAI `image_url` content part.
    pub fn data_uri(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.jpeg);
        format!("data:image/jpeg;base64,{b64}")
    }
}

/// The backend seam: turn a prepared image + prompt into description text (or an error).
/// `Send + Sync` so it can be shared onto the describe worker thread.
pub trait Describer: Send + Sync {
    fn describe(&self, image: &PreparedImage, prompt: &str) -> Result<String, DescribeError>;
}

/// Downscale display-oriented RGBA8 to [`MAX_EDGE`] and JPEG-encode it for the endpoint.
pub fn prepare_image(rgba: Vec<u8>, w: u32, h: u32) -> Result<PreparedImage, DescribeError> {
    let (rgba, w, h) = if w > MAX_EDGE || h > MAX_EDGE {
        pb_decode::downscale_rgba8(
            rgba,
            w,
            h,
            pb_decode::FitBox {
                max_width: MAX_EDGE,
                max_height: MAX_EDGE,
            },
        )
        .map_err(|e| DescribeError::Prep(e.to_string()))?
    } else {
        (rgba, w, h)
    };
    let jpeg = pb_encode::encode_jpeg_rgba8(&rgba, w, h, JPEG_QUALITY)
        .map_err(|e| DescribeError::Prep(e.to_string()))?;
    Ok(PreparedImage { jpeg })
}

/// The whole worker-thread job for the endpoint backend: decode `frame` of `item` at full
/// resolution (the still, or the video/animation frame on screen — task #81), tone-map to
/// sRGB8 + bake the displayed rotation (as the clipboard/scan paths do), prepare the image,
/// then hand it to `describer`. Mirrors
/// [`image_text::scan_job`](crate::image_text::scan_job); the event loop never blocks.
pub fn describe_job(
    source: &dyn ItemSource,
    item: usize,
    frame: &crate::frame_capture::FrameSource,
    rot: Rotation,
    prompt: &str,
    describer: &dyn Describer,
) -> Result<String, DescribeError> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let img = crate::frame_capture::capture(source, item, frame, &cancel)
        .map_err(|e| DescribeError::Prep(format!("can't read this image ({e})")))?;
    let rgba = to_clipboard_rgba8(&img);
    let (rgba, w, h) = rotate_rgba8(&rgba, img.width, img.height, rot);
    let prepared = prepare_image(rgba, w, h)?;
    describer.describe(&prepared, prompt)
}

/// The OpenAI-compatible local-endpoint backend.
#[derive(Clone, Debug)]
pub struct LocalEndpoint {
    /// Base URL, e.g. `http://localhost:1234/v1` (LM Studio's default).
    pub base_url: String,
    /// Model name; empty → omit the field (LM Studio serves the loaded model).
    pub model: String,
    /// Response length cap (max tokens) — the settings' `describe_max_tokens`.
    pub max_tokens: u32,
}

impl LocalEndpoint {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            max_tokens,
        }
    }
}

impl Describer for LocalEndpoint {
    fn describe(&self, image: &PreparedImage, prompt: &str) -> Result<String, DescribeError> {
        let url = chat_url(&self.base_url);
        let data_uri = image.data_uri();
        let preset = if self.max_tokens == 0 {
            MAX_TOKENS
        } else {
            self.max_tokens
        };
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            .timeout_global(Some(std::time::Duration::from_secs(GLOBAL_TIMEOUT_SECS)))
            // Read 4xx/5xx bodies ourselves: the server's error message is the most
            // useful thing a failed describe can show.
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let key = thinker_key(&self.base_url, &self.model);
        let mut thinks = thinkers().lock().is_ok_and(|t| t.contains(&key));
        let result = run_adaptive(preset, &mut thinks, &mut |max_tokens| {
            let body = build_chat_request(&self.model, prompt, &data_uri, max_tokens);
            let mut resp = agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send_json(&body)
                .map_err(map_ureq_error)?;
            let status = resp.status().as_u16();
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| DescribeError::BadResponse(e.to_string()))?;
            if !(200..300).contains(&status) {
                return Err(DescribeError::BadStatus(
                    status,
                    server_error_message(&text),
                ));
            }
            parse_chat_reply(&text)
        });
        if thinks {
            if let Ok(mut t) = thinkers().lock() {
                t.insert(key);
            }
        }
        result
    }
}

/// The endpoint + model pairs seen **thinking** this session, which get
/// [`REASONING_HEADROOM`] on the first try from then on. RAM only, and keyed by server +
/// model id — server behaviour, never what was viewed — so it dies with the process like
/// every other cache (ADR-018).
fn thinkers() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static THINKERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    THINKERS.get_or_init(Default::default)
}

fn thinker_key(base_url: &str, model: &str) -> String {
    format!("{}\u{0}{}", normalize_base(base_url), model.trim())
}

/// The describe loop, transport-free so it's unit-testable. Thinking is the model's (and
/// the user's) choice, so Blaze never asks a model not to think — it makes room instead.
/// The length preset budgets the *answer*; a model that thinks also gets
/// [`REASONING_HEADROOM`]. Until a model is seen thinking it is sent the plain preset, so
/// a model that doesn't think keeps the preset as its exact cap. One that **runs out while
/// thinking** is retried once with the headroom (at most two requests — the retry is a
/// one-way change), and `thinks` is set so the caller can remember it.
fn run_adaptive(
    preset: u32,
    thinks: &mut bool,
    send: &mut dyn FnMut(u32) -> Result<ChatReply, DescribeError>,
) -> Result<String, DescribeError> {
    let roomy = preset.saturating_add(REASONING_HEADROOM);
    let mut max_tokens = if *thinks { roomy } else { preset };
    loop {
        let reply = send(max_tokens)?;
        *thinks |= reply.reasoned;
        if reply.ran_out_thinking() && max_tokens < roomy {
            max_tokens = roomy;
            continue;
        }
        return reply.into_description();
    }
}

/// Normalize the configured endpoint: trim, default a missing scheme to `http://`, drop
/// trailing slashes, and **append `/v1` when the URL has no path at all**. LM Studio's
/// server panel says "reachable at http://host:1234" without the version segment, and
/// every OpenAI-compatible server we target (LM Studio, Ollama, llama.cpp, vLLM) serves
/// the API under `/v1`. A URL that already has any path is left alone, so a server
/// mounted elsewhere still works.
pub fn normalize_base(base: &str) -> String {
    let t = base.trim().trim_end_matches('/');
    if t.is_empty() {
        return String::new();
    }
    let url = if t.contains("://") {
        t.to_string()
    } else {
        format!("http://{t}")
    };
    let has_path = url
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'));
    if has_path {
        url
    } else {
        format!("{url}/v1")
    }
}

/// Join the configured base URL with the chat-completions path, tolerating a trailing
/// slash, a missing `/v1` ([`normalize_base`]) and a base that already points straight at
/// the endpoint.
fn chat_url(base: &str) -> String {
    let base = normalize_base(base);
    if base.ends_with("/chat/completions") {
        base
    } else {
        format!("{base}/chat/completions")
    }
}

/// Build the OpenAI chat-completions request body: one user message carrying the prompt
/// text and the image as a data-URI `image_url` part. `model` is omitted when empty;
/// `max_tokens` caps thinking + answer together ([`run_adaptive`] sizes it).
fn build_chat_request(model: &str, prompt: &str, data_uri: &str, max_tokens: u32) -> Value {
    let mut req = json!({
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]
        }],
        "max_tokens": max_tokens,
        "temperature": 0.2,
        "stream": false
    });
    if !model.trim().is_empty() {
        req["model"] = json!(model.trim());
    }
    req
}

/// The **Test connection** result both Settings screens show: reachability, model count,
/// whether any model looks vision-capable, and — when [`normalize_base`] changed what was
/// typed — the URL actually used, so a missing `/v1` is fixed visibly, not silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    /// Green (usable) vs red (unreachable, or no vision model to describe with).
    pub ok: bool,
    pub message: String,
    /// The served model ids, vision-capable first ([`sort_models_vision_first`]).
    pub models: Vec<String>,
}

/// Run the **Test connection** probe and summarize it ([`ProbeReport`]). Blocking — call
/// it off the UI thread.
pub fn probe_report(endpoint: &str) -> ProbeReport {
    summarize_probe(endpoint, probe_endpoint(endpoint))
}

fn summarize_probe(endpoint: &str, probe: Result<Vec<String>, DescribeError>) -> ProbeReport {
    let models = match probe {
        Err(e) => {
            return ProbeReport {
                ok: false,
                message: e.user_message(),
                models: Vec::new(),
            }
        }
        Ok(models) => models,
    };
    let normalized = normalize_base(endpoint);
    let using = if normalized == endpoint.trim().trim_end_matches('/') {
        String::new()
    } else {
        format!(" · using {normalized}")
    };
    if models.is_empty() {
        return ProbeReport {
            ok: false,
            message: format!("Reachable, but no models are loaded{using}."),
            models,
        };
    }
    let n = models.len();
    let plural = if n == 1 { "" } else { "s" };
    let has_vision = models.iter().any(|m| looks_like_vision_model(m));
    let message = if has_vision {
        format!("Reachable · {n} model{plural} · vision model present{using}")
    } else {
        format!(
            "Reachable · {n} model{plural}, but none look vision-capable — describe needs a \
             VLM (e.g. qwen2.5-vl){using}."
        )
    };
    ProbeReport {
        ok: has_vision,
        message,
        models: sort_models_vision_first(models),
    }
}

/// Probe an endpoint for the **Test connection** button: `GET {base}/models`, returning
/// the served model ids (or a taxonomy error). Confirms the server is reachable and lets
/// the UI flag when no listed model looks vision-capable (describe needs a VLM).
pub fn probe_endpoint(base_url: &str) -> Result<Vec<String>, DescribeError> {
    let base = normalize_base(base_url);
    let url = if base.ends_with("/models") {
        base
    } else {
        format!("{base}/models")
    };
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let text = agent
        .get(&url)
        .call()
        .map_err(map_ureq_error)?
        .body_mut()
        .read_to_string()
        .map_err(|e| DescribeError::BadResponse(e.to_string()))?;
    let v: Value =
        serde_json::from_str(&text).map_err(|e| DescribeError::BadResponse(e.to_string()))?;
    let ids: Vec<String> = v
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ids)
}

/// Stable-sort a model list so **vision-capable** ids come first (by
/// [`looks_like_vision_model`]), each group keeping its original order. The AI settings model
/// picker uses this so the usable VLMs surface above the text-only models an endpoint also
/// serves (LM Studio commonly lists a dozen text models beside one VLM).
pub fn sort_models_vision_first(mut models: Vec<String>) -> Vec<String> {
    // `false` (vision) sorts before `true`; `sort_by_key` is stable, so intra-group order holds.
    models.sort_by_key(|m| !looks_like_vision_model(m));
    models
}

/// Heuristic: does a model id look like a **vision-language** model? Used only to warn in
/// the Test-connection result — describe needs a VLM, and a text-only model silently
/// ignores the image. Matches the common families; deliberately loose (a false negative
/// just suppresses a hint, never blocks a describe).
pub fn looks_like_vision_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    [
        "vl",
        "vision",
        "llava",
        "pixtral",
        "moondream",
        "-vlm",
        "bakllava",
        "cogvlm",
        "internvl",
    ]
    .iter()
    .any(|k| id.contains(k))
}

/// One chat-completions reply, parsed but not yet judged ([`ChatReply::into_description`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ChatReply {
    /// The visible answer, trimmed, with any inline `<think>` block removed ("" if none).
    content: String,
    /// `choices[0].finish_reason` — `"length"` means the cap cut the generation off.
    finish_reason: Option<String>,
    /// The model spent tokens thinking: a `reasoning_content` / `reasoning` field, an
    /// inline `<think>` block, or `usage…reasoning_tokens` > 0.
    reasoned: bool,
}

impl ChatReply {
    fn hit_limit(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }

    /// Still thinking when the cap arrived: no answer, but a reason to retry with room.
    fn ran_out_thinking(&self) -> bool {
        self.content.is_empty() && self.hit_limit() && self.reasoned
    }

    fn into_description(self) -> Result<String, DescribeError> {
        if !self.content.is_empty() {
            // Cut off mid-answer by the cap: mark it rather than end mid-sentence silently.
            return Ok(if self.hit_limit() {
                format!("{}…", self.content)
            } else {
                self.content
            });
        }
        Err(if self.ran_out_thinking() {
            DescribeError::ThoughtTooLong
        } else {
            DescribeError::EmptyReply
        })
    }
}

/// Parse a chat-completions reply. Accepts the usual string `content`, the array-of-parts
/// form some servers emit, and a `null` content (a thinking model that never answered);
/// surfaces a server `error` object as [`DescribeError::ServerError`].
fn parse_chat_reply(text: &str) -> Result<ChatReply, DescribeError> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| DescribeError::BadResponse("the reply isn't JSON".to_string()))?;
    if let Some(msg) = error_message(&v) {
        return Err(DescribeError::ServerError(msg));
    }
    let choice = v.get("choices").and_then(|c| c.get(0));
    let message = choice
        .and_then(|c| c.get("message"))
        .ok_or_else(|| DescribeError::BadResponse("it has no choices[0].message".to_string()))?;
    let raw = match message.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        // Array-of-parts: concatenate every part's `text`.
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        Some(_) => {
            return Err(DescribeError::BadResponse(
                "its content isn't text".to_string(),
            ))
        }
    };
    let (content, inline_thought) = strip_think_block(&raw);
    let has_text = |key: &str| {
        message
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    let reasoning_tokens = v
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(ChatReply {
        content,
        finish_reason: choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        reasoned: inline_thought
            || has_text("reasoning_content")
            || has_text("reasoning")
            || reasoning_tokens > 0,
    })
}

/// Remove a leading `<think>…</think>` block — servers that don't split reasoning into
/// its own field leave it inline. An unclosed block is all thought (the cap arrived
/// mid-thinking), so nothing is left. Returns the trimmed answer and whether a block was
/// found.
fn strip_think_block(raw: &str) -> (String, bool) {
    let s = raw.trim();
    let Some(rest) = s.strip_prefix("<think>") else {
        return (s.to_string(), false);
    };
    let answer = rest
        .split_once("</think>")
        .map_or("", |(_, answer)| answer.trim());
    (answer.to_string(), true)
}

/// The server's own error text from a JSON body: `{"error": {"message": "…"}}` (OpenAI)
/// or `{"error": "…"}` (LM Studio).
fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    e.get("message")
        .unwrap_or(e)
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The error text in a non-2xx body, if it's JSON carrying one.
fn server_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(error_message)
}

/// Map a [`ureq::Error`] onto the coarse [`DescribeError`] taxonomy.
fn map_ureq_error(e: ureq::Error) -> DescribeError {
    match e {
        ureq::Error::StatusCode(code) => DescribeError::BadStatus(code, None),
        ureq::Error::Timeout(_) => DescribeError::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => DescribeError::Unreachable,
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => DescribeError::NotConfigured,
        ureq::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            // "No route to host" / "network is unreachable" — a dead LAN host, or, on macOS,
            // a Local Network permission the app hasn't been granted (EHOSTUNREACH to a LAN IP).
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::TimedOut => DescribeError::Unreachable,
            _ => DescribeError::Backend(format!("network error: {io}")),
        },
        other => DescribeError::Backend(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned [`Describer`] for the state-machine tests (subtask 3): returns a fixed
    /// reply, or a fixed error, and records that it was asked.
    pub struct FakeDescriber {
        pub reply: Result<String, DescribeError>,
    }

    impl Describer for FakeDescriber {
        fn describe(&self, _image: &PreparedImage, _prompt: &str) -> Result<String, DescribeError> {
            self.reply.clone()
        }
    }

    // --- chat_url ------------------------------------------------------------------

    #[test]
    fn chat_url_appends_the_path_and_tolerates_a_trailing_slash() {
        assert_eq!(
            chat_url("http://localhost:1234/v1"),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            chat_url("http://localhost:1234/v1/"),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_leaves_a_full_endpoint_url_alone() {
        let full = "http://host:9/v1/chat/completions";
        assert_eq!(chat_url(full), full);
        assert_eq!(
            chat_url("  http://host:9/v1  "),
            "http://host:9/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_adds_the_missing_v1() {
        // What LM Studio's server panel shows ("reachable at …") — no version segment.
        assert_eq!(
            chat_url("http://192.168.1.123:1234"),
            "http://192.168.1.123:1234/v1/chat/completions"
        );
    }

    // --- normalize_base ------------------------------------------------------------

    #[test]
    fn normalize_base_appends_v1_only_when_there_is_no_path() {
        assert_eq!(
            normalize_base("http://192.168.1.123:1234"),
            "http://192.168.1.123:1234/v1"
        );
        assert_eq!(
            normalize_base(" http://host:1234/ "),
            "http://host:1234/v1",
            "a bare trailing slash is not a path"
        );
        assert_eq!(
            normalize_base("http://host:1234/v1/"),
            "http://host:1234/v1"
        );
        assert_eq!(
            normalize_base("https://example.com/openai"),
            "https://example.com/openai",
            "a server mounted elsewhere keeps its path"
        );
    }

    #[test]
    fn normalize_base_defaults_a_missing_scheme_to_http() {
        assert_eq!(normalize_base("localhost:1234"), "http://localhost:1234/v1");
        assert_eq!(
            normalize_base("gremlin.local:1234/v1"),
            "http://gremlin.local:1234/v1"
        );
        assert_eq!(normalize_base("   "), "");
    }

    #[test]
    fn thinker_key_treats_equivalent_urls_as_one_server() {
        assert_eq!(
            thinker_key("http://h:1234", " m "),
            thinker_key("http://h:1234/v1/", "m")
        );
        assert_ne!(
            thinker_key("http://h:1234", "a"),
            thinker_key("http://h:1234", "b")
        );
    }

    // --- build_chat_request --------------------------------------------------------

    #[test]
    fn request_carries_prompt_and_image_and_omits_empty_model() {
        let req = build_chat_request("", "Describe it.", "data:image/jpeg;base64,AAAA", 512);
        assert!(req.get("model").is_none(), "empty model is omitted");
        let content = &req["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Describe it.");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
        assert_eq!(req["max_tokens"], 512);
        assert_eq!(req["stream"], false);
        // Thinking is the model's call: nothing asks it to skip.
        assert!(req.get("reasoning_effort").is_none());
    }

    #[test]
    fn request_includes_a_named_model_and_length_cap() {
        let req = build_chat_request("qwen2-vl", "hi", "data:,", 1024);
        assert_eq!(req["model"], "qwen2-vl");
        assert_eq!(req["max_tokens"], 1024);
    }

    // --- run_adaptive --------------------------------------------------------------

    fn answered(text: &str, reasoned: bool) -> ChatReply {
        ChatReply {
            content: text.to_string(),
            finish_reason: Some("stop".to_string()),
            reasoned,
        }
    }

    /// What LM Studio returned for qwen3.6 when the cap ran out mid-thought.
    fn out_of_room() -> ChatReply {
        ChatReply {
            content: String::new(),
            finish_reason: Some("length".to_string()),
            reasoned: true,
        }
    }

    /// Run the loop over a scripted server, returning the result and every cap sent.
    fn script(
        thinks: &mut bool,
        replies: Vec<Result<ChatReply, DescribeError>>,
    ) -> (Result<String, DescribeError>, Vec<u32>) {
        let mut replies = replies.into_iter();
        let mut sent = Vec::new();
        let result = run_adaptive(512, thinks, &mut |max_tokens| {
            sent.push(max_tokens);
            replies
                .next()
                .expect("the loop sent more requests than scripted")
        });
        (result, sent)
    }

    #[test]
    fn adaptive_keeps_the_exact_preset_for_a_model_that_doesnt_think() {
        let mut thinks = false;
        let (result, sent) = script(&mut thinks, vec![Ok(answered("A cat.", false))]);
        assert_eq!(result.unwrap(), "A cat.");
        assert_eq!(sent, vec![512]);
        assert!(!thinks);
    }

    #[test]
    fn adaptive_retries_with_headroom_when_the_model_runs_out_thinking() {
        let mut thinks = false;
        let (result, sent) = script(
            &mut thinks,
            vec![Ok(out_of_room()), Ok(answered("A dog.", true))],
        );
        assert_eq!(result.unwrap(), "A dog.");
        assert_eq!(sent, vec![512, 512 + REASONING_HEADROOM]);
        assert!(
            thinks,
            "remembered, so the next describe gets the room up front"
        );

        let (_, sent) = script(&mut thinks, vec![Ok(answered("Again.", true))]);
        assert_eq!(sent, vec![512 + REASONING_HEADROOM]);
    }

    #[test]
    fn adaptive_remembers_a_thinker_that_fit_the_first_time() {
        // A short think that fit in the preset still marks the model, so a longer think
        // on the next image doesn't fail first.
        let mut thinks = false;
        let (result, sent) = script(&mut thinks, vec![Ok(answered("Some text.", true))]);
        assert_eq!(result.unwrap(), "Some text.");
        assert_eq!(sent, vec![512]);
        assert!(thinks);
    }

    #[test]
    fn adaptive_gives_up_with_a_clear_error_when_even_the_headroom_runs_out() {
        let mut thinks = false;
        let (result, sent) = script(&mut thinks, vec![Ok(out_of_room()), Ok(out_of_room())]);
        assert_eq!(result, Err(DescribeError::ThoughtTooLong));
        assert_eq!(sent.len(), 2, "one headroom retry, never a loop");
    }

    #[test]
    fn adaptive_never_retries_a_failed_request() {
        let mut thinks = false;
        let refusal = DescribeError::BadStatus(400, Some("Model does not support images".into()));
        let (result, sent) = script(&mut thinks, vec![Err(refusal.clone())]);
        assert_eq!(result, Err(refusal));
        assert_eq!(sent.len(), 1);
    }

    // --- probe_report --------------------------------------------------------------

    #[test]
    fn probe_report_says_when_it_added_v1() {
        let models = || Ok(vec!["qwen3-32b".to_string(), "qwen2.5-vl-32b".to_string()]);
        let r = summarize_probe("http://192.168.1.123:1234", models());
        assert!(r.ok);
        assert_eq!(
            r.message,
            "Reachable · 2 models · vision model present · using http://192.168.1.123:1234/v1"
        );
        assert_eq!(
            r.models,
            vec!["qwen2.5-vl-32b", "qwen3-32b"],
            "vision first"
        );
        let r = summarize_probe("http://192.168.1.123:1234/v1/", models());
        assert_eq!(r.message, "Reachable · 2 models · vision model present");
    }

    #[test]
    fn probe_report_flags_no_models_and_no_vision_models() {
        let r = summarize_probe("http://h:1/v1", Ok(vec![]));
        assert!(!r.ok);
        assert_eq!(r.message, "Reachable, but no models are loaded.");
        let r = summarize_probe("http://h:1/v1", Ok(vec!["gpt-oss-20b".to_string()]));
        assert!(!r.ok);
        assert!(
            r.message.contains("none look vision-capable"),
            "{}",
            r.message
        );
        let r = summarize_probe("http://h:1/v1", Err(DescribeError::Timeout));
        assert_eq!(r.message, DescribeError::Timeout.user_message());
    }

    #[test]
    fn sort_models_vision_first_surfaces_vlms_stably() {
        let models = vec![
            "meta-llama-3.1-70b".to_string(),
            "qwen2.5-vl-32b".to_string(),
            "gpt-oss-20b".to_string(),
            "llava-7b".to_string(),
        ];
        assert_eq!(
            sort_models_vision_first(models),
            vec![
                "qwen2.5-vl-32b",
                "llava-7b",
                "meta-llama-3.1-70b",
                "gpt-oss-20b"
            ],
            "vision models first, original order preserved within each group"
        );
    }

    #[test]
    fn vision_model_heuristic_matches_common_families() {
        assert!(looks_like_vision_model("qwen_qwen2.5-vl-32b-instruct"));
        assert!(looks_like_vision_model("llava-v1.6-mistral-7b"));
        assert!(looks_like_vision_model("Pixtral-12B"));
        assert!(!looks_like_vision_model("meta-llama-3.1-70b-instruct"));
        assert!(!looks_like_vision_model("openai/gpt-oss-120b"));
    }

    // --- parse_chat_reply ----------------------------------------------------------

    /// Parse and judge, the way `describe` does for a reply it won't retry.
    fn describe_text(body: &str) -> Result<String, DescribeError> {
        parse_chat_reply(body)?.into_description()
    }

    #[test]
    fn parse_pulls_string_content() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"  A cat.  "}}]}"#;
        assert_eq!(describe_text(body).unwrap(), "A cat.");
    }

    #[test]
    fn parse_concatenates_array_content_parts() {
        let body = r#"{"choices":[{"message":{"content":[
            {"type":"text","text":"A "},{"type":"text","text":"dog."}]}}]}"#;
        assert_eq!(describe_text(body).unwrap(), "A dog.");
    }

    #[test]
    fn parse_surfaces_a_server_error_object_in_either_shape() {
        assert_eq!(
            describe_text(r#"{"error":{"message":"model not found"}}"#),
            Err(DescribeError::ServerError("model not found".to_string()))
        );
        assert_eq!(
            server_error_message(r#"{"error":"Model does not support images."}"#).as_deref(),
            Some("Model does not support images.")
        );
        assert_eq!(server_error_message("<html>Bad Gateway</html>"), None);
    }

    #[test]
    fn parse_rejects_what_isnt_a_chat_completion() {
        assert!(matches!(
            describe_text("not json"),
            Err(DescribeError::BadResponse(_))
        ));
        assert!(matches!(
            describe_text(r#"{"choices":[]}"#),
            Err(DescribeError::BadResponse(_))
        ));
        assert_eq!(
            describe_text(r#"{"choices":[{"message":{"content":"   "},"finish_reason":"stop"}]}"#),
            Err(DescribeError::EmptyReply)
        );
    }

    /// The exact shape LM Studio sent for qwen3.6-35b-a3b at a 300-token cap (2026-11-14):
    /// the whole budget spent in `reasoning_content`, `content` empty. This used to surface
    /// as "Couldn't read the endpoint's reply (empty description)".
    #[test]
    fn parse_recognizes_a_model_that_ran_out_while_thinking() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"",
            "reasoning_content":"The user wants a description…","tool_calls":[]},
            "finish_reason":"length"}],"usage":{"prompt_tokens":24,"completion_tokens":300,
            "total_tokens":324,"completion_tokens_details":{"reasoning_tokens":268}}}"#;
        let reply = parse_chat_reply(body).unwrap();
        assert!(reply.ran_out_thinking());
        assert_eq!(reply.into_description(), Err(DescribeError::ThoughtTooLong));
        // A null content (some servers) and a usage-only signal read the same way.
        let body = r#"{"choices":[{"message":{"content":null},"finish_reason":"length"}],
            "usage":{"completion_tokens_details":{"reasoning_tokens":10}}}"#;
        assert!(parse_chat_reply(body).unwrap().ran_out_thinking());
    }

    #[test]
    fn parse_strips_an_inline_think_block() {
        let body = r#"{"choices":[{"message":{"content":"<think>hmm</think>\n A red square."},
            "finish_reason":"stop"}]}"#;
        let reply = parse_chat_reply(body).unwrap();
        assert!(reply.reasoned);
        assert_eq!(reply.into_description().unwrap(), "A red square.");
        // Unclosed: the cap arrived mid-thought — no answer, not the thought as the answer.
        let body = r#"{"choices":[{"message":{"content":"<think>hmm, the image"},
            "finish_reason":"length"}]}"#;
        assert!(parse_chat_reply(body).unwrap().ran_out_thinking());
    }

    #[test]
    fn a_cut_off_answer_is_marked_not_silently_truncated() {
        let body = r#"{"choices":[{"message":{"content":"A long description that"},
            "finish_reason":"length"}]}"#;
        assert_eq!(describe_text(body).unwrap(), "A long description that…");
    }

    // --- prepare_image + data URI --------------------------------------------------

    #[test]
    fn prepare_image_downscales_and_encodes_jpeg() {
        // 2000×1000 opaque tile → downscaled to ≤1024 long edge, JPEG-encoded.
        let (w, h) = (2000u32, 1000u32);
        let rgba = vec![120u8; (w * h * 4) as usize];
        let prepared = prepare_image(rgba, w, h).unwrap();
        assert_eq!(&prepared.jpeg[..2], &[0xFF, 0xD8], "valid JPEG SOI");
        let uri = prepared.data_uri();
        assert!(uri.starts_with("data:image/jpeg;base64,"));
        assert!(uri.len() > "data:image/jpeg;base64,".len());
    }

    #[test]
    fn prepare_image_rejects_a_bad_buffer() {
        assert!(matches!(
            prepare_image(vec![0; 3], 10, 10,),
            Err(DescribeError::Prep(_))
        ));
    }

    // --- error mapping / messages --------------------------------------------------

    #[test]
    fn ureq_status_and_timeout_map_to_taxonomy() {
        assert_eq!(
            map_ureq_error(ureq::Error::StatusCode(404)),
            DescribeError::BadStatus(404, None)
        );
        assert_eq!(
            map_ureq_error(ureq::Error::HostNotFound),
            DescribeError::Unreachable
        );
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(
            map_ureq_error(ureq::Error::Io(refused)),
            DescribeError::Unreachable
        );
    }

    #[test]
    fn user_messages_are_actionable() {
        let unreachable = DescribeError::Unreachable.user_message();
        assert!(unreachable.contains("model server"));
        #[cfg(target_os = "macos")]
        assert!(
            unreachable.contains("Local Network"),
            "hints at the macOS permission"
        );
        assert!(DescribeError::BadStatus(500, None)
            .user_message()
            .contains("500"));
        assert!(DescribeError::BadStatus(404, None)
            .user_message()
            .contains("/v1"));
        let refused = DescribeError::BadStatus(400, Some("Model does not support images.".into()));
        assert!(refused
            .user_message()
            .ends_with("Model does not support images."));
        assert!(DescribeError::ThoughtTooLong
            .user_message()
            .contains("thinks less"));
        assert_eq!(
            DescribeError::Backend("custom".to_string()).user_message(),
            "custom"
        );
    }

    #[test]
    fn fake_describer_returns_its_canned_reply() {
        let d = FakeDescriber {
            reply: Ok("hello".to_string()),
        };
        let img = PreparedImage {
            jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
        };
        assert_eq!(d.describe(&img, "prompt").unwrap(), "hello");
    }

    #[test]
    fn probe_endpoint_reports_unreachable_fast_for_a_dead_host() {
        // A closed port on localhost refuses immediately — the Test button's fail path.
        let r = probe_endpoint("http://127.0.0.1:1/v1");
        assert!(
            matches!(
                r,
                Err(DescribeError::Unreachable | DescribeError::Backend(_))
            ),
            "{r:?}"
        );
    }
}
