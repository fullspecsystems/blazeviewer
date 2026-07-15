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

use crate::engine::{decode_item, rotate_rgba8, to_clipboard_rgba8};

/// An in-flight off-thread describe for `item` — the [`image_text::TextScan`] shape: kicked
/// by `D` / Ask, polled by the tick, superseded by dropping the receiver. `gen` is the deck
/// generation at kick time, so a late result after a playlist rebuild is discarded.
///
/// [`image_text::TextScan`]: crate::image_text::TextScan
pub struct DescribeScan {
    pub gen: u64,
    pub item: usize,
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
    /// The endpoint answered with a non-2xx HTTP status.
    BadStatus(u16),
    /// The reply parsed as HTTP but not as the expected chat-completion shape (or carried
    /// an `error` object).
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
            DescribeError::BadStatus(code) => {
                format!("The endpoint returned an error (HTTP {code}).")
            }
            DescribeError::BadResponse(why) => {
                format!("Couldn't read the endpoint's reply ({why}).")
            }
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
    let jpeg = pb_decode::encode_jpeg_rgba8(&rgba, w, h, JPEG_QUALITY)
        .map_err(|e| DescribeError::Prep(e.to_string()))?;
    Ok(PreparedImage { jpeg })
}

/// The whole worker-thread job for the endpoint backend: decode `item` at full
/// resolution, tone-map to sRGB8 + bake the displayed rotation (as the clipboard/scan
/// paths do), prepare the image, then hand it to `describer`. Mirrors
/// [`image_text::scan_job`](crate::image_text::scan_job); the event loop never blocks.
pub fn describe_job(
    source: &dyn ItemSource,
    item: usize,
    rot: Rotation,
    prompt: &str,
    describer: &dyn Describer,
) -> Result<String, DescribeError> {
    let img = decode_item(source, item, None, false)
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
        let body = build_chat_request(&self.model, prompt, &image.data_uri(), self.max_tokens);
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            .timeout_global(Some(std::time::Duration::from_secs(GLOBAL_TIMEOUT_SECS)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut resp = agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(map_ureq_error)?;
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| DescribeError::BadResponse(e.to_string()))?;
        parse_chat_response(&text)
    }
}

/// Join the configured base URL with the chat-completions path, tolerating a trailing
/// slash and a base that already points straight at the endpoint.
fn chat_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

/// Build the OpenAI chat-completions request body: one user message carrying the prompt
/// text and the image as a data-URI `image_url` part. `model` is omitted when empty;
/// `max_tokens` is the response-length cap (0 falls back to [`MAX_TOKENS`]).
fn build_chat_request(model: &str, prompt: &str, data_uri: &str, max_tokens: u32) -> Value {
    let mut req = json!({
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]
        }],
        "max_tokens": if max_tokens == 0 { MAX_TOKENS } else { max_tokens },
        "temperature": 0.2,
        "stream": false
    });
    if !model.trim().is_empty() {
        req["model"] = json!(model.trim());
    }
    req
}

/// Probe an endpoint for the **Test connection** button: `GET {base}/models`, returning
/// the served model ids (or a taxonomy error). Confirms the server is reachable and lets
/// the UI flag when no listed model looks vision-capable (describe needs a VLM).
pub fn probe_endpoint(base_url: &str) -> Result<Vec<String>, DescribeError> {
    let base = base_url.trim().trim_end_matches('/');
    let url = if base.ends_with("/models") {
        base.to_string()
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

/// Pull the assistant's text out of a chat-completions reply. Accepts the usual string
/// `content`, and the array-of-parts form some servers emit; surfaces a server `error`
/// object as a [`DescribeError::BadResponse`].
fn parse_chat_response(text: &str) -> Result<String, DescribeError> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| DescribeError::BadResponse(e.to_string()))?;
    if let Some(msg) = v
        .get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(Value::as_str)
    {
        return Err(DescribeError::BadResponse(msg.to_string()));
    }
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .ok_or_else(|| DescribeError::BadResponse("no choices[0].message.content".to_string()))?;
    let out = match content {
        Value::String(s) => s.trim().to_string(),
        // Array-of-parts: concatenate every part's `text`.
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string(),
        _ => {
            return Err(DescribeError::BadResponse(
                "content wasn't text".to_string(),
            ))
        }
    };
    if out.is_empty() {
        return Err(DescribeError::BadResponse("empty description".to_string()));
    }
    Ok(out)
}

/// Map a [`ureq::Error`] onto the coarse [`DescribeError`] taxonomy.
fn map_ureq_error(e: ureq::Error) -> DescribeError {
    match e {
        ureq::Error::StatusCode(code) => DescribeError::BadStatus(code),
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
    }

    #[test]
    fn request_includes_a_named_model_and_length_cap() {
        let req = build_chat_request("qwen2-vl", "hi", "data:,", 1024);
        assert_eq!(req["model"], "qwen2-vl");
        assert_eq!(req["max_tokens"], 1024);
        // A zero cap falls back to the built-in default.
        assert_eq!(
            build_chat_request("", "hi", "data:,", 0)["max_tokens"],
            MAX_TOKENS
        );
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

    // --- parse_chat_response -------------------------------------------------------

    #[test]
    fn parse_pulls_string_content() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"  A cat.  "}}]}"#;
        assert_eq!(parse_chat_response(body).unwrap(), "A cat.");
    }

    #[test]
    fn parse_concatenates_array_content_parts() {
        let body = r#"{"choices":[{"message":{"content":[
            {"type":"text","text":"A "},{"type":"text","text":"dog."}]}}]}"#;
        assert_eq!(parse_chat_response(body).unwrap(), "A dog.");
    }

    #[test]
    fn parse_surfaces_a_server_error_object() {
        let body = r#"{"error":{"message":"model not found"}}"#;
        assert_eq!(
            parse_chat_response(body),
            Err(DescribeError::BadResponse("model not found".to_string()))
        );
    }

    #[test]
    fn parse_rejects_malformed_or_empty() {
        assert!(matches!(
            parse_chat_response("not json"),
            Err(DescribeError::BadResponse(_))
        ));
        assert!(matches!(
            parse_chat_response(r#"{"choices":[]}"#),
            Err(DescribeError::BadResponse(_))
        ));
        assert!(matches!(
            parse_chat_response(r#"{"choices":[{"message":{"content":"   "}}]}"#),
            Err(DescribeError::BadResponse(_))
        ));
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
            DescribeError::BadStatus(404)
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
        assert!(DescribeError::BadStatus(500).user_message().contains("500"));
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
