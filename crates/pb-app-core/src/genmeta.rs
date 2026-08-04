//! AI generation metadata — ComfyUI + Automatic1111 (task #137).
//!
//! Turns the raw text payloads [`pb_decode::read_png_text`] extracts into one
//! shell-neutral [`GenerationMeta`] the Details panel can render. Pure: no I/O,
//! no clock, no shell — the whole file unit-tests exhaustively.
//!
//! # The safety invariant
//!
//! > **A fact is emitted only when every viable candidate agrees. Otherwise it
//! > is omitted and reported as unknown.**
//!
//! This is the entire design. A wrong seed or a wrong prompt is worse than a
//! blank row, because the user cannot tell it is wrong — they will paste it into
//! a generator and get something else. Every rule below exists to keep a
//! *plausible* answer from being reported as a *known* one.
//!
//! # Two things that are counter-intuitive and were both learned from real files
//!
//! 1. **Parse `prompt`, never `workflow`.** ComfyUI writes two graphs. `prompt`
//!    is the API graph that *executed*; `workflow` is the editor's document,
//!    which also contains muted and bypassed nodes that did **not** run. The
//!    corpus this was built against has a muted `CLIPTextEncode` holding a
//!    perfectly plausible positive prompt (`"mode": 4`) that had nothing to do
//!    with the image. Scanning `workflow` — which most third-party tools do —
//!    reports that prompt confidently and wrongly. `workflow` is a copy payload
//!    only.
//! 2. **Parse graph *shape*, not node types.** The traversal keys on input
//!    *names* (`images`, `positive`, `model`, `text`), never `class_type`. The
//!    corpus routes its pixels through `CR Upscale Image` from a third-party
//!    pack, and the walk crosses it without knowing anything about it, because
//!    the input name `image` is conventional. There are thousands of custom node
//!    packs; enumerating them is not possible, and not needed.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

/// Which tool wrote the metadata. There is deliberately no `Unknown` arm —
/// "we could not tell" is [`parse`] returning `None`, not a variant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenTool {
    ComfyUI,
    Automatic1111,
}

impl GenTool {
    pub fn name(self) -> &'static str {
        match self {
            GenTool::ComfyUI => "ComfyUI",
            GenTool::Automatic1111 => "Automatic1111",
        }
    }
}

/// How well we know a prompt.
///
/// There is no `Resolved` arm on purpose. An earlier design followed links
/// looking for "any string literal", which can just as easily find a filename
/// prefix, a delimiter, a style name, or one arbitrary alternative out of a
/// combinator's list — and would then label that guess as the prompt. Either the
/// graph states the text literally or we say we do not know it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PromptSource {
    /// Read straight off the conditioning node (or the A1111 parameters block).
    Literal,
    /// The text arrived over a link, so it is not stored in the file at all.
    /// `via` names the source node's `class_type`, which is genuinely useful:
    /// it tells the user *why* their prompt is missing.
    Unresolved { via: String },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PromptText {
    /// `None` whenever [`source`](PromptText::source) is
    /// [`PromptSource::Unresolved`] — an unknown prompt carries no text rather
    /// than a guess.
    pub text: Option<String>,
    pub source: PromptSource,
}

impl PromptText {
    fn literal(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            source: PromptSource::Literal,
        }
    }

    fn unresolved(via: impl Into<String>) -> Self {
        Self {
            text: None,
            source: PromptSource::Unresolved { via: via.into() },
        }
    }

    /// The one-line explanation shown in place of a prompt we could not read.
    pub fn unresolved_reason(&self) -> Option<String> {
        match &self.source {
            PromptSource::Literal => None,
            PromptSource::Unresolved { via } => {
                Some(format!("not stored literally — assembled by {via}"))
            }
        }
    }
}

/// Everything we could establish about how an image was generated.
///
/// Every field is independently optional: a graph that defeats the model walk
/// still reports its prompts, and one with an unresolvable prompt still reports
/// its seed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GenerationMeta {
    pub tool: GenTool,
    pub positive: Option<PromptText>,
    pub negative: Option<PromptText>,
    pub model: Option<String>,
    /// `(name, strength summary)`, in the order the model chain passes them.
    pub loras: Vec<(String, String)>,
    /// Display-ready `(label, value)` facts: Seed, Steps, CFG, Sampler, Size.
    pub params: Vec<(String, String)>,
    /// One line per additional sampler beyond the base, ancestry-ordered.
    pub passes: Vec<String>,
    /// A raw payload exists, so the copy command is worth offering, even when
    /// no fact above could be derived.
    pub has_payload: bool,
}

impl GenerationMeta {
    fn empty(tool: GenTool) -> Self {
        Self {
            tool,
            positive: None,
            negative: None,
            model: None,
            loras: Vec::new(),
            params: Vec::new(),
            passes: Vec::new(),
            has_payload: true,
        }
    }

    /// Whether anything beyond "a payload exists" was established. A panel can
    /// use this to say "metadata present but not readable" honestly.
    pub fn has_facts(&self) -> bool {
        self.positive.is_some()
            || self.negative.is_some()
            || self.model.is_some()
            || !self.loras.is_empty()
            || !self.params.is_empty()
            || !self.passes.is_empty()
    }
}

/// A prompt longer than this is stored truncated. `GenerationMeta` is cached
/// per item in an unbounded map, so an uncapped prompt is an unbounded cache.
/// Real prompts are a few hundred bytes; 16 KB is far past any real one.
const MAX_PROMPT: usize = 16 * 1024;

/// Refuse to walk a graph larger than this. Bounds the work on the event loop
/// and stops a hostile file from turning a panel open into a hang.
const MAX_NODES: usize = 4096;

/// How deep the ancestry walk may go before giving up.
const MAX_DEPTH: usize = 256;

/// Input names that carry pixels or latents — the edges the ancestry walk
/// follows. Deliberately *names*, so unknown custom nodes are traversable.
const IMAGE_INPUTS: [&str; 5] = ["images", "image", "samples", "latent_image", "pixels"];

/// Parse whatever generation metadata `chunks` and `user_comment` carry.
///
/// `chunks` is [`pb_decode::read_png_text`]'s output; `user_comment` is
/// [`pb_decode::read_exif_user_comment`]'s, for the JPEG/WebP A1111 case.
/// Returns `None` when neither carries anything we recognize — which is the
/// overwhelmingly common case, and must stay cheap.
pub fn parse(chunks: &[(String, String)], user_comment: Option<&str>) -> Option<GenerationMeta> {
    let get = |k: &str| {
        chunks
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    // ComfyUI first: `prompt` is the executed graph. `workflow` is deliberately
    // NOT required — an image can carry one without the other, and demanding
    // both would refuse files whose payload we can still copy.
    if let Some(prompt) = get("prompt") {
        return Some(parse_comfy(prompt));
    }
    if get("workflow").is_some() {
        // A UI graph with no API graph: no facts are derivable (see the module
        // docs), but the payload is real and worth offering.
        return Some(GenerationMeta::empty(GenTool::ComfyUI));
    }
    if let Some(params) = get("parameters") {
        return Some(parse_a1111(params));
    }
    if let Some(uc) = user_comment.filter(|s| looks_like_a1111(s)) {
        return Some(parse_a1111(uc));
    }
    None
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_PROMPT {
        text.to_string()
    } else {
        let mut end = MAX_PROMPT;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    }
}

// ---------------------------------------------------------------------------
// Automatic1111 family
// ---------------------------------------------------------------------------

/// Keys that mark a line as A1111's trailing parameter record. Any one of them
/// starting the line is enough; all real writers emit `Steps:` first.
const A1111_KEYS: [&str; 4] = ["Steps: ", "Sampler: ", "Seed: ", "CFG scale: "];

/// The params we surface, in display order, mapped to our labels. Anything else
/// A1111 wrote stays in the payload for the copy command.
const A1111_SHOWN: [(&str, &str); 5] = [
    ("Seed", "Seed"),
    ("Steps", "Steps"),
    ("CFG scale", "CFG"),
    ("Sampler", "Sampler"),
    ("Size", "Size"),
];

fn looks_like_a1111(text: &str) -> bool {
    find_param_record(text).is_some()
}

/// The index of the trailing parameter record, if the text has one.
///
/// It must be the **last** line and start with a known key. Scanning for the
/// *first* `Steps:` would misfire on a prompt that mentions steps — prompts are
/// free text and can contain anything, including a line that looks like the
/// record.
fn find_param_record(text: &str) -> Option<usize> {
    let last_line_start = text.trim_end().rfind('\n').map_or(0, |i| i + 1);
    let line = text[last_line_start..].trim();
    A1111_KEYS
        .iter()
        .any(|k| line.starts_with(k))
        .then_some(last_line_start)
}

fn parse_a1111(text: &str) -> GenerationMeta {
    let mut meta = GenerationMeta::empty(GenTool::Automatic1111);
    let (body, record) = match find_param_record(text) {
        Some(at) => (&text[..at], Some(text[at..].trim())),
        // No parameter record at all: the whole payload is the prompt. Better
        // than reporting nothing, and it cannot be wrong — there is nothing
        // else it could be.
        None => (text, None),
    };
    // Split at the LAST negative-prompt marker above the record: a positive
    // prompt may itself contain the words "Negative prompt:" on a line.
    let (pos, neg) = match body.rfind("\nNegative prompt: ") {
        Some(at) => (
            &body[..at],
            Some(body[at + "\nNegative prompt: ".len()..].trim()),
        ),
        None => (body, None),
    };
    let pos = pos.trim();
    if !pos.is_empty() {
        meta.positive = Some(PromptText::literal(truncate(pos)));
    }
    if let Some(neg) = neg.filter(|n| !n.is_empty()) {
        meta.negative = Some(PromptText::literal(truncate(neg)));
    }
    if let Some(record) = record {
        let fields = split_param_record(record);
        for (key, label) in A1111_SHOWN {
            if let Some(v) = fields.get(key) {
                meta.params.push((label.to_string(), v.clone()));
            }
        }
        // The model is named by `Model`, with `Model hash` as the fallback for
        // writers that omit the name.
        meta.model = fields
            .get("Model")
            .or_else(|| fields.get("Model hash"))
            .cloned();
    }
    meta
}

/// Split A1111's `Key: value, Key: value` record.
///
/// Naively splitting on `,` is wrong: values contain commas (`Lora hashes:
/// "a: 1, b: 2"`, `Size: 512x768` is safe but `TI hashes` is not). A separator
/// only counts when what follows looks like a fresh `Key: ` — a short run of
/// letters, digits and spaces ending in a colon.
fn split_param_record(record: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let bytes = record.as_bytes();
    let mut starts = vec![0usize];
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b',' && bytes[i + 1] == b' ' && starts_a_key(&record[i + 2..]) {
            starts.push(i + 2);
            i += 2;
        } else {
            i += 1;
        }
    }
    starts.push(record.len());
    for w in starts.windows(2) {
        let field = record[w[0]..w[1]].trim_end_matches(", ").trim();
        if let Some((k, v)) = field.split_once(": ") {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// Whether `s` opens with a plausible `Key: ` token.
fn starts_a_key(s: &str) -> bool {
    let mut seen = 0;
    for c in s.chars() {
        match c {
            ':' => return seen > 0 && s[seen..].starts_with(": "),
            c if c.is_ascii_alphanumeric() || c == ' ' || c == '_' => {
                seen += c.len_utf8();
                if seen > 32 {
                    return false; // too long to be a key
                }
            }
            _ => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// ComfyUI
// ---------------------------------------------------------------------------

/// One node of the API graph, in the only shape the walk cares about.
struct Node<'a> {
    class_type: &'a str,
    inputs: &'a serde_json::Map<String, Value>,
}

impl<'a> Node<'a> {
    /// A scalar (non-link) input as a display string, if it is one.
    fn literal(&self, name: &str) -> Option<String> {
        match self.inputs.get(name)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// The node id a linked input points at, if it is a link (`[id, slot]`).
    fn link(&self, name: &str) -> Option<&'a str> {
        self.inputs.get(name)?.as_array()?.first()?.as_str()
    }

    /// Does this node have `name` at all, in either form?
    fn has(&self, name: &str) -> bool {
        self.inputs.contains_key(name)
    }

    /// A sampler is defined by its **conditioning shape**, not its class name or
    /// its widgets: it consumes a positive and a negative conditioning and a
    /// latent. Testing for a `steps` input instead — as an earlier design did —
    /// misfires on detailers, upscalers, restorers and interpolators, all of
    /// which expose `steps` and none of which is the thing that made the image.
    fn is_sampler(&self) -> bool {
        self.has("positive") && self.has("negative") && self.has("latent_image")
    }
}

type Graph<'a> = HashMap<&'a str, Node<'a>>;

fn parse_comfy(json: &str) -> GenerationMeta {
    let mut meta = GenerationMeta::empty(GenTool::ComfyUI);
    let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(json) else {
        return meta;
    };
    if raw.len() > MAX_NODES {
        return meta;
    }
    let graph: Graph = raw
        .iter()
        .filter_map(|(id, v)| {
            let obj = v.as_object()?;
            Some((
                id.as_str(),
                Node {
                    class_type: obj.get("class_type")?.as_str()?,
                    inputs: obj.get("inputs")?.as_object()?,
                },
            ))
        })
        .collect();

    // Terminals: every SaveImage. `SaveImage` is a stable core class name, so
    // this is the one place a name is trusted.
    let mut terminals: Vec<&str> = graph
        .iter()
        .filter(|(_, n)| n.class_type == "SaveImage")
        .map(|(id, _)| *id)
        .collect();
    terminals.sort_unstable();
    if terminals.is_empty() {
        // No terminal means no way to know which branch produced this file.
        // Picking an arbitrary unconsumed sink would be fabrication.
        return meta;
    }

    // Every terminal's sampler ancestry, deepest-first. With several save nodes
    // the facts are emitted only where all of them agree.
    let chains: Vec<Vec<&str>> = terminals
        .iter()
        .map(|t| sampler_ancestry(&graph, t))
        .collect();
    let Some(chain) = agree(&chains) else {
        return meta; // terminals disagree — omit rather than choose
    };
    if chain.is_empty() {
        return meta;
    }

    // The base pass is the deepest sampler, but only when there is exactly one
    // deepest candidate. In img2img/inpaint the deepest sampler may only build
    // an *input* to the real generation, so a tie means no headline.
    let base = chain[0];
    let Some(node) = graph.get(base) else {
        return meta;
    };

    if let Some((p, n)) = conditioning(&graph, node) {
        meta.positive = p;
        meta.negative = n;
    }
    for (input, label) in [
        ("seed", "Seed"),
        ("steps", "Steps"),
        ("cfg", "CFG"),
        ("sampler_name", "Sampler"),
        ("scheduler", "Scheduler"),
    ] {
        if let Some(v) = scalar(&graph, node, input) {
            meta.params.push((label.to_string(), v));
        }
    }
    if let Some(size) = latent_size(&graph, node) {
        meta.params.push(("Size".to_string(), size));
    }
    let (model, loras) = model_chain(&graph, node);
    meta.model = model;
    meta.loras = loras;

    // Additional passes, in execution order (deepest first is base, so the rest
    // follow the pixels toward the save node).
    for id in chain.iter().skip(1) {
        if let Some(n) = graph.get(id) {
            meta.passes.push(describe_pass(&graph, n));
        }
    }
    meta
}

/// The sampler ids on `terminal`'s ancestry, deepest first.
///
/// Traverses **all** image-like inputs, not one: the graph is a DAG, and a
/// composite or blend has several image parents. Following only the first would
/// silently pick a mask or reference layer.
fn sampler_ancestry<'a>(graph: &Graph<'a>, terminal: &'a str) -> Vec<&'a str> {
    let mut order: Vec<&'a str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    // Depth-first, recording depth so the result can be ordered deepest-first.
    let mut stack = vec![(terminal, 0usize)];
    let mut depths: Vec<(usize, &'a str)> = Vec::new();
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_DEPTH || !seen.insert(id) {
            continue;
        }
        let Some(node) = graph.get(id) else { continue };
        if node.is_sampler() {
            depths.push((depth, id));
        }
        for name in IMAGE_INPUTS {
            if let Some(src) = node.link(name) {
                stack.push((src, depth + 1));
            }
        }
    }
    depths.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    for (_, id) in depths {
        order.push(id);
    }
    order
}

/// The one chain all candidates agree on, or `None` if they differ.
fn agree<'a>(chains: &[Vec<&'a str>]) -> Option<Vec<&'a str>> {
    let first = chains.first()?;
    chains.iter().all(|c| c == first).then(|| first.clone())
}

/// A sampler's positive and negative prompts.
///
/// Follows the conditioning link one hop to the node that encodes it, then reads
/// its `text`. A literal is the prompt; a link means the text is not in the file
/// (see [`PromptSource`]).
fn conditioning(graph: &Graph, sampler: &Node) -> Option<(Option<PromptText>, Option<PromptText>)> {
    let read = |name: &str| -> Option<PromptText> {
        let id = sampler.link(name)?;
        let node = graph.get(id)?;
        match node.literal("text") {
            Some(t) if !t.trim().is_empty() => Some(PromptText::literal(truncate(t.trim()))),
            Some(_) => None, // an empty literal is a real, empty prompt: say nothing
            None => {
                // `text` arrived over a link (or is absent). Name the source so
                // the user learns why their prompt is missing.
                let via = node
                    .link("text")
                    .and_then(|src| graph.get(src))
                    .map(|n| n.class_type)
                    .unwrap_or(node.class_type);
                Some(PromptText::unresolved(via))
            }
        }
    };
    Some((read("positive"), read("negative")))
}

/// A sampler's scalar input, resolving a link **only** by exact input-name match.
///
/// The corpus wires its seed in from a dedicated `easy seed` node, so refusing
/// all links would lose the single most-wanted fact. But following a link and
/// taking whatever literal is handy is how you report a batch index as a seed.
/// The narrow rule — the source node must carry a literal under the *same* name
/// — covers the seed-node idiom and nothing else.
fn scalar(graph: &Graph, node: &Node, name: &str) -> Option<String> {
    if let Some(v) = node.literal(name) {
        return Some(v);
    }
    let src = graph.get(node.link(name)?)?;
    src.literal(name)
}

/// The latent dimensions, from a genuine chain head only.
///
/// A node with literal `width`/`height` that also consumes an image is a crop or
/// an upscale, and its numbers are not the latent size.
fn latent_size(graph: &Graph, sampler: &Node) -> Option<String> {
    let mut id = sampler.link("latent_image")?;
    for _ in 0..MAX_DEPTH {
        let node = graph.get(id)?;
        let consumes_image = IMAGE_INPUTS.iter().any(|n| node.has(n));
        if !consumes_image {
            let (w, h) = (node.literal("width")?, node.literal("height")?);
            return Some(format!("{w} × {h}"));
        }
        id = IMAGE_INPUTS.iter().find_map(|n| node.link(n))?;
    }
    None
}

/// The checkpoint and the LoRAs on a sampler's model chain.
///
/// Stops and reports **no** model if the chain forks — a node with two or more
/// `model` inputs is a merge, and naming one side would be a coin flip.
fn model_chain(graph: &Graph, sampler: &Node) -> (Option<String>, Vec<(String, String)>) {
    let mut loras = Vec::new();
    let mut id = match sampler.link("model") {
        Some(id) => id,
        None => return (None, loras),
    };
    for _ in 0..MAX_DEPTH {
        let Some(node) = graph.get(id) else {
            return (None, loras);
        };
        let model_inputs = node
            .inputs
            .keys()
            .filter(|k| k.starts_with("model"))
            .count();
        if model_inputs > 1 {
            return (None, loras); // a merge: refuse rather than pick a side
        }
        if let Some(name) = node.literal("lora_name") {
            let strength = node
                .literal("strength_model")
                .map(|s| format!("strength {s}"))
                .unwrap_or_default();
            loras.push((name, strength));
        }
        for key in ["ckpt_name", "unet_name", "model_name"] {
            if let Some(name) = node.literal(key) {
                return (Some(name), loras);
            }
        }
        match node.link("model") {
            Some(next) => id = next,
            None => return (None, loras),
        }
    }
    (None, loras)
}

/// A refinement pass as one line: `30 steps, cfg 5.5, denoise 0.31`.
fn describe_pass(graph: &Graph, node: &Node) -> String {
    let mut parts = Vec::new();
    if let Some(s) = scalar(graph, node, "steps") {
        parts.push(format!("{s} steps"));
    }
    if let Some(c) = scalar(graph, node, "cfg") {
        parts.push(format!("cfg {c}"));
    }
    if let Some(d) = scalar(graph, node, "denoise") {
        parts.push(format!("denoise {d}"));
    }
    if parts.is_empty() {
        "refinement pass".to_string()
    } else {
        parts.join(", ")
    }
}

/// The Details panel's **Generation** block, or empty when there is nothing
/// worth showing (task #137).
///
/// Pure, so the panel and the Copy Image Details command can share one
/// definition and cannot disagree about the same file — the shape
/// [`crate::tracks::track_rows`] already establishes for video tracks.
///
/// An unreadable prompt still produces a row. Saying *why* it is missing
/// ("assembled by PromptCombinator") is the honest outcome, and far more useful
/// than a blank space the user reads as a bug.
pub fn detail_rows(meta: &GenerationMeta) -> Vec<crate::panels::DetailRow> {
    use crate::action::Action;
    use crate::panels::{DetailRow, RowAction};
    let mut rows = Vec::new();
    let span = |text: String, bold: bool| DetailRow::Span { text, bold };

    // The section heading's buttons. **Copy prompt only appears when there is a
    // literal prompt to copy** — offering a button whose sole outcome is a
    // refusal toast is worse than not offering it, and the panel already says
    // why the prompt is missing right underneath. Copy data is always offered:
    // reaching this function at all means a payload exists.
    let heading = |text: String, meta: &GenerationMeta| {
        let mut actions = Vec::new();
        if meta
            .positive
            .as_ref()
            .is_some_and(|p| p.text.as_ref().is_some_and(|t| !t.is_empty()))
        {
            actions.push(RowAction {
                label: "Copy prompt".to_string(),
                action: Action::CopyGenerationPrompt,
            });
        }
        actions.push(RowAction {
            label: "Copy data".to_string(),
            action: Action::CopyGenerationData,
        });
        DetailRow::Section { text, actions }
    };

    if !meta.has_facts() {
        // A payload we cannot read anything out of. Say so plainly rather than
        // showing nothing, so the Copy command does not look like it does
        // nothing when the user reaches for it.
        rows.push(heading(format!("Generation ({})", meta.tool.name()), meta));
        rows.push(DetailRow::Pair {
            label: "Details".to_string(),
            value: "present but not readable — copy it with the button above".to_string(),
        });
        return rows;
    }

    rows.push(heading(format!("Generation ({})", meta.tool.name()), meta));
    // A prompt we HAVE gets a bold heading over a full-width paragraph; a prompt we
    // do not gets a label/value pair, because the value is a short note rather than
    // content and belongs in the facts table. Both are labelled either way — an
    // unlabelled paragraph under "Generation" leaves the reader guessing which
    // prompt they are looking at, and the two are not interchangeable.
    let mut prompt_rows =
        |heading: &str, label: &str, p: &PromptText| match (&p.text, p.unresolved_reason()) {
            (Some(text), _) => {
                rows.push(span(heading.to_string(), true));
                rows.push(DetailRow::Body { text: text.clone() });
            }
            (None, Some(why)) => rows.push(DetailRow::Pair {
                label: label.to_string(),
                value: why,
            }),
            (None, None) => {}
        };
    if let Some(p) = &meta.positive {
        prompt_rows("Prompt", "Prompt", p);
    }
    if let Some(n) = &meta.negative {
        prompt_rows("Negative prompt", "Negative", n);
    }
    let mut pair = |label: &str, value: String| {
        rows.push(DetailRow::Pair {
            label: label.to_string(),
            value,
        })
    };
    if let Some(model) = &meta.model {
        pair("Model", model.clone());
    }
    for (name, strength) in &meta.loras {
        pair(
            "LoRA",
            if strength.is_empty() {
                name.clone()
            } else {
                format!("{name} ({strength})")
            },
        );
    }
    for (label, value) in &meta.params {
        pair(label, value.clone());
    }
    for (i, p) in meta.passes.iter().enumerate() {
        pair(&format!("Pass {}", i + 2), p.clone());
    }
    rows
}

#[cfg(test)]
mod tests;
