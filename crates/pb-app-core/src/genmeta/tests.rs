//! `genmeta` tests (task #137).
//!
//! Every test here defends the safety invariant — *a fact is emitted only when
//! every viable candidate agrees* — against one specific way a greedy parser
//! would emit a **wrong** fact instead of no fact. The fixtures are
//! hand-authored graph *shapes*, not real files: the shape is what the parser
//! reasons about, and the corpus that motivated this is not committed.

use super::*;

fn chunks(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn comfy(json: &str) -> GenerationMeta {
    parse(&chunks(&[("prompt", json)]), None).expect("a `prompt` chunk is ComfyUI")
}

fn param(meta: &GenerationMeta, label: &str) -> Option<String> {
    meta.params
        .iter()
        .find(|(l, _)| l == label)
        .map(|(_, v)| v.clone())
}

/// A minimal, unambiguous graph: one sampler, literal prompts, one checkpoint.
const SIMPLE: &str = r#"{
  "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "sdxl.safetensors"}},
  "2": {"class_type": "CLIPTextEncode", "inputs": {"text": "a red bird", "clip": ["1", 1]}},
  "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry", "clip": ["1", 1]}},
  "4": {"class_type": "EmptyLatentImage", "inputs": {"width": 832, "height": 1216}},
  "5": {"class_type": "KSampler", "inputs": {
        "seed": 42, "steps": 30, "cfg": 7.0, "sampler_name": "euler", "scheduler": "normal",
        "model": ["1", 0], "positive": ["2", 0], "negative": ["3", 0], "latent_image": ["4", 0]}},
  "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0]}},
  "7": {"class_type": "SaveImage", "inputs": {"images": ["6", 0]}}
}"#;

#[test]
fn reads_an_unambiguous_graph() {
    let m = comfy(SIMPLE);
    assert_eq!(m.tool, GenTool::ComfyUI);
    assert_eq!(m.positive, Some(PromptText::literal("a red bird")));
    assert_eq!(m.negative, Some(PromptText::literal("blurry")));
    assert_eq!(m.model.as_deref(), Some("sdxl.safetensors"));
    assert_eq!(param(&m, "Seed").as_deref(), Some("42"));
    assert_eq!(param(&m, "Steps").as_deref(), Some("30"));
    assert_eq!(param(&m, "CFG").as_deref(), Some("7.0"));
    assert_eq!(param(&m, "Sampler").as_deref(), Some("euler"));
    assert_eq!(param(&m, "Size").as_deref(), Some("832 × 1216"));
    assert!(m.passes.is_empty());
    assert!(m.has_facts());
}

#[test]
fn a_muted_node_in_workflow_is_never_reported_as_the_prompt() {
    // THE headline regression. ComfyUI's `workflow` is the editor document and
    // contains muted/bypassed nodes that did not run; the corpus has one
    // holding a plausible positive prompt (`"mode": 4`). Only `prompt`, the
    // executed API graph, may supply facts.
    let workflow = r#"{"nodes": [
        {"id": 88, "type": "CLIPTextEncode", "mode": 4,
         "widgets_values": ["a prompt that never ran"]}]}"#;
    let m = parse(&chunks(&[("prompt", SIMPLE), ("workflow", workflow)]), None).unwrap();
    assert_eq!(m.positive, Some(PromptText::literal("a red bird")));
    let rendered = format!("{m:?}");
    assert!(
        !rendered.contains("never ran"),
        "a muted workflow node leaked into the facts: {rendered}"
    );
}

#[test]
fn a_workflow_without_a_prompt_offers_a_payload_but_no_facts() {
    // The UI graph alone cannot tell us what executed, but the user can still
    // copy it — so requiring both keys (an earlier design) would refuse a file
    // whose payload is perfectly usable.
    let m = parse(&chunks(&[("workflow", r#"{"nodes": []}"#)]), None).unwrap();
    assert!(m.has_payload);
    assert!(!m.has_facts());
    assert_eq!(m.tool, GenTool::ComfyUI);
}

#[test]
fn disagreeing_save_nodes_emit_no_facts() {
    // Two SaveImage nodes over different samplers: node ids are identifiers,
    // not output order, so "first by id" would report the other branch's seed
    // for this file. Disagreement means omit.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "KSampler", "inputs": {"seed": 999, "steps": 20,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "4": {"class_type": "SaveImage", "inputs": {"images": ["2", 0]}},
      "5": {"class_type": "SaveImage", "inputs": {"images": ["3", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert!(
        !m.has_facts(),
        "disagreeing terminals must yield no facts: {m:?}"
    );
    assert!(m.has_payload, "the payload is still copyable");
}

#[test]
fn agreeing_save_nodes_still_report() {
    // Two terminals fed by the SAME sampler agree, so the facts stand — the
    // rule is agreement, not "more than one terminal is fatal".
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 7, "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "4": {"class_type": "SaveImage", "inputs": {"images": ["2", 0]}},
      "5": {"class_type": "SaveImage", "inputs": {"images": ["2", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    assert_eq!(param(&comfy(json), "Seed").as_deref(), Some("7"));
}

#[test]
fn a_graph_with_no_save_node_emits_no_facts() {
    // Choosing an arbitrary unconsumed sink is fabrication: a graph can have
    // many, and nothing says which one is this file.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 5, "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "PreviewImage", "inputs": {"images": ["2", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert!(!m.has_facts());
    assert!(m.has_payload);
}

#[test]
fn a_detailer_with_steps_is_not_treated_as_a_sampler() {
    // Detailers, upscalers, restorers and interpolators all expose `steps`.
    // A sampler is defined by its conditioning shape (positive + negative +
    // latent_image), so the detailer's 8 steps must not become the headline.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 3, "steps": 40,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "SomeDetailer", "inputs": {"steps": 8, "image": ["2", 0]}},
      "4": {"class_type": "SaveImage", "inputs": {"images": ["3", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert_eq!(param(&m, "Steps").as_deref(), Some("40"));
    assert!(
        m.passes.is_empty(),
        "the detailer is not a pass: {:?}",
        m.passes
    );
}

#[test]
fn a_linked_prompt_is_unresolved_and_carries_no_text() {
    // The corpus's own shape: `text` arrives from a PromptCombinator emitting a
    // combinatorial list, so the string that made THIS image is not in the
    // file. Hopping links for "any literal" would have grabbed one alternative
    // (or a delimiter) and labelled it the prompt.
    let json = r#"{
      "1": {"class_type": "PromptCombinator", "inputs": {"input_list_1": "girl", "input_list_2": "blonde"}},
      "2": {"class_type": "CLIPTextEncode", "inputs": {"text": ["1", 0]}},
      "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry"}},
      "4": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "5": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
            "positive": ["2", 0], "negative": ["3", 0], "latent_image": ["4", 0]}},
      "6": {"class_type": "SaveImage", "inputs": {"images": ["5", 0]}}
    }"#;
    let m = comfy(json);
    let pos = m
        .positive
        .clone()
        .expect("an unresolved prompt is still reported");
    assert_eq!(pos.text, None, "an unresolved prompt must carry no guess");
    assert_eq!(
        pos.source,
        PromptSource::Unresolved {
            via: "PromptCombinator".into()
        }
    );
    // Just the cause — the row is labelled "Prompt" and rendered as a note, so a
    // "not stored literally" preamble only repeats what the presentation says.
    assert_eq!(
        pos.unresolved_reason().as_deref(),
        Some("Assembled by PromptCombinator")
    );
    // The negative was literal and is unaffected — facts are independent.
    assert_eq!(m.negative, Some(PromptText::literal("blurry")));
    // And nothing from the combinator's word lists leaked into the output.
    let rendered = format!("{m:?}");
    assert!(
        !rendered.contains("girl") && !rendered.contains("blonde"),
        "{rendered}"
    );
}

#[test]
fn a_linked_seed_resolves_only_by_input_name_match() {
    // The `easy seed` idiom is ubiquitous and wiring the seed in is normal, so
    // refusing all links would lose the most-wanted fact. But the match must be
    // by NAME: a source node's unrelated literal (a batch index, a step count)
    // must never be reported as the seed.
    let matched = r#"{
      "0": {"class_type": "easy seed", "inputs": {"seed": 98767}},
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": ["0", 0], "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "SaveImage", "inputs": {"images": ["2", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    assert_eq!(param(&comfy(matched), "Seed").as_deref(), Some("98767"));

    let mismatched = matched.replace(r#""seed": 98767"#, r#""batch_index": 4"#);
    let m = comfy(&mismatched);
    assert_eq!(param(&m, "Seed"), None, "a name mismatch must not resolve");
    let rendered = format!("{m:?}");
    assert!(
        !rendered.contains('4') || !rendered.contains("Seed"),
        "{rendered}"
    );
}

#[test]
fn a_forked_model_chain_reports_no_model() {
    // A model merge has two model parents; naming one is a coin flip presented
    // as a fact. LoRAs found before the fork are still reported.
    let json = r#"{
      "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "a.safetensors"}},
      "2": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "b.safetensors"}},
      "3": {"class_type": "ModelMergeSimple", "inputs": {"model1": ["1", 0], "model2": ["2", 0]}},
      "4": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "5": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10, "model": ["3", 0],
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["4", 0]}},
      "6": {"class_type": "SaveImage", "inputs": {"images": ["5", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert_eq!(m.model, None, "a merge must not name a side");
    assert_eq!(
        param(&m, "Seed").as_deref(),
        Some("1"),
        "other facts survive"
    );
}

#[test]
fn loras_are_collected_along_an_unforked_chain() {
    let json = r#"{
      "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "base.safetensors"}},
      "2": {"class_type": "LoraLoader", "inputs": {"lora_name": "detail.safetensors",
            "strength_model": 0.8, "model": ["1", 0]}},
      "3": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "4": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10, "model": ["2", 0],
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["3", 0]}},
      "5": {"class_type": "SaveImage", "inputs": {"images": ["4", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert_eq!(m.model.as_deref(), Some("base.safetensors"));
    assert_eq!(
        m.loras,
        vec![("detail.safetensors".to_string(), "strength 0.8".to_string())]
    );
}

#[test]
fn the_base_pass_not_the_refiner_supplies_the_headline_numbers() {
    // The corpus's two-pass shape: base 35 steps / denoise 1.0, then a hi-res
    // refiner at 30 steps / denoise 0.31 across an upscale bridge. Reporting
    // "the sampler" naively shows the refiner — wrong in the way that is hard
    // to notice, because 30 steps is a perfectly plausible number.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 832, "height": 1216}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 5, "steps": 35, "cfg": 6.0, "denoise": 1.0,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "VAEDecode", "inputs": {"samples": ["2", 0]}},
      "4": {"class_type": "CR Upscale Image", "inputs": {"image": ["3", 0]}},
      "5": {"class_type": "VAEEncode", "inputs": {"pixels": ["4", 0]}},
      "6": {"class_type": "KSampler", "inputs": {"seed": 5, "steps": 30, "cfg": 5.5, "denoise": 0.31,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["5", 0]}},
      "7": {"class_type": "VAEDecode", "inputs": {"samples": ["6", 0]}},
      "8": {"class_type": "SaveImage", "inputs": {"images": ["7", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    let m = comfy(json);
    assert_eq!(
        param(&m, "Steps").as_deref(),
        Some("35"),
        "base, not refiner"
    );
    assert_eq!(param(&m, "CFG").as_deref(), Some("6.0"));
    assert_eq!(param(&m, "Size").as_deref(), Some("832 × 1216"));
    assert_eq!(
        m.passes,
        vec!["30 steps, cfg 5.5, denoise 0.31".to_string()]
    );
}

#[test]
fn the_walk_crosses_unknown_custom_nodes() {
    // `CR Upscale Image` above is from a third-party pack. The traversal keys
    // on the input name `image`, never on class names — there are thousands of
    // packs and enumerating them is not possible.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "2": {"class_type": "KSampler", "inputs": {"seed": 11, "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["1", 0]}},
      "3": {"class_type": "🔧 Totally Unknown Node", "inputs": {"image": ["2", 0], "flavour": "x"}},
      "4": {"class_type": "SaveImage", "inputs": {"images": ["3", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    assert_eq!(param(&comfy(json), "Seed").as_deref(), Some("11"));
}

#[test]
fn a_crop_node_does_not_supply_the_latent_size() {
    // A node carrying literal width/height that ALSO consumes an image is a
    // crop or an upscale; its numbers are the output size, not the latent.
    let json = r#"{
      "1": {"class_type": "EmptyLatentImage", "inputs": {"width": 832, "height": 1216}},
      "2": {"class_type": "LatentCrop", "inputs": {"width": 512, "height": 512, "samples": ["1", 0]}},
      "3": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
            "positive": ["9", 0], "negative": ["9", 0], "latent_image": ["2", 0]}},
      "4": {"class_type": "SaveImage", "inputs": {"images": ["3", 0]}},
      "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "x"}}
    }"#;
    assert_eq!(param(&comfy(json), "Size").as_deref(), Some("832 × 1216"));
}

#[test]
fn a_cyclic_graph_terminates() {
    // A hand-made cycle must end the walk, not overflow the stack.
    let json = r#"{
      "1": {"class_type": "VAEDecode", "inputs": {"samples": ["2", 0]}},
      "2": {"class_type": "VAEEncode", "inputs": {"pixels": ["1", 0]}},
      "3": {"class_type": "SaveImage", "inputs": {"images": ["1", 0]}}
    }"#;
    let m = comfy(json);
    assert!(!m.has_facts());
}

#[test]
fn malformed_and_oversize_input_never_panics() {
    for bad in [
        "",
        "null",
        "[]",
        "not json at all",
        "{}",
        r#"{"1": 4}"#,
        r#"{"1": {"class_type": "SaveImage"}}"#,
        r#"{"1": {"inputs": {}}}"#,
    ] {
        let m = parse(&chunks(&[("prompt", bad)]), None).expect("a prompt key is still ComfyUI");
        assert!(!m.has_facts(), "{bad} produced facts: {m:?}");
    }
    // A graph past MAX_NODES is refused rather than walked.
    let huge: String = (0..MAX_NODES + 1)
        .map(|i| format!(r#""{i}": {{"class_type": "SaveImage", "inputs": {{}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let m = comfy(&format!("{{{huge}}}"));
    assert!(!m.has_facts());
}

#[test]
fn nothing_recognizable_is_none() {
    assert!(parse(&[], None).is_none());
    assert!(parse(&chunks(&[("Software", "gimp")]), None).is_none());
    assert!(parse(&[], Some("just a caption")).is_none());
}

#[test]
fn a_prompt_longer_than_the_cap_is_truncated() {
    // GenerationMeta is cached per item in an unbounded map, so an uncapped
    // prompt is an unbounded cache.
    let long = "a".repeat(MAX_PROMPT * 2);
    let json = format!(
        r#"{{
          "1": {{"class_type": "CLIPTextEncode", "inputs": {{"text": "{long}"}}}},
          "2": {{"class_type": "EmptyLatentImage", "inputs": {{"width": 64, "height": 64}}}},
          "3": {{"class_type": "KSampler", "inputs": {{"seed": 1, "steps": 10,
                "positive": ["1", 0], "negative": ["1", 0], "latent_image": ["2", 0]}}}},
          "4": {{"class_type": "SaveImage", "inputs": {{"images": ["3", 0]}}}}
        }}"#
    );
    let text = comfy(&json).positive.unwrap().text.unwrap();
    assert!(text.len() <= MAX_PROMPT + 4, "capped, got {}", text.len());
    assert!(text.ends_with('…'));
}

// ---------------------------------------------------------------------------
// Automatic1111
// ---------------------------------------------------------------------------

#[test]
fn reads_the_a1111_parameters_block() {
    let text = "a red bird on a wire\n\
                Negative prompt: blurry, watermark\n\
                Steps: 20, Sampler: DPM++ 2M, CFG scale: 7, Seed: 12345, Size: 512x768, Model: sd_xl_base";
    let m = parse(&chunks(&[("parameters", text)]), None).unwrap();
    assert_eq!(m.tool, GenTool::Automatic1111);
    assert_eq!(
        m.positive,
        Some(PromptText::literal("a red bird on a wire"))
    );
    assert_eq!(m.negative, Some(PromptText::literal("blurry, watermark")));
    assert_eq!(m.model.as_deref(), Some("sd_xl_base"));
    assert_eq!(param(&m, "Seed").as_deref(), Some("12345"));
    assert_eq!(param(&m, "Steps").as_deref(), Some("20"));
    assert_eq!(param(&m, "CFG").as_deref(), Some("7"));
    assert_eq!(param(&m, "Sampler").as_deref(), Some("DPM++ 2M"));
    assert_eq!(param(&m, "Size").as_deref(), Some("512x768"));
}

#[test]
fn a1111_without_a_negative_prompt() {
    let text = "just a positive\nSteps: 20, Sampler: Euler";
    let m = parse(&chunks(&[("parameters", text)]), None).unwrap();
    assert_eq!(m.positive, Some(PromptText::literal("just a positive")));
    assert_eq!(m.negative, None);
}

#[test]
fn a1111_values_may_contain_commas() {
    // Naive comma splitting shreds quoted hash lists into garbage keys, and a
    // shredded record can silently drop or corrupt the real Seed.
    let text = "cat\n\
                Steps: 20, Sampler: Euler a, Seed: 7, \
                Lora hashes: \"add_detail: aaaa, add_sharp: bbbb\", Size: 512x512";
    let m = parse(&chunks(&[("parameters", text)]), None).unwrap();
    assert_eq!(param(&m, "Seed").as_deref(), Some("7"));
    assert_eq!(param(&m, "Sampler").as_deref(), Some("Euler a"));
    assert_eq!(param(&m, "Size").as_deref(), Some("512x512"));
}

#[test]
fn a_prompt_containing_marker_lines_does_not_confuse_the_split() {
    // Prompts are free text: one can legitimately contain a line starting
    // "Steps:" or "Negative prompt:". The record is the LAST line, and the
    // negative split takes the LAST marker.
    let text = "a diagram, Steps: not a real record\n\
                Negative prompt: first\n\
                Negative prompt: second\n\
                Steps: 20, Seed: 3";
    let m = parse(&chunks(&[("parameters", text)]), None).unwrap();
    assert_eq!(param(&m, "Seed").as_deref(), Some("3"));
    assert_eq!(m.negative, Some(PromptText::literal("second")));
    assert!(m
        .positive
        .unwrap()
        .text
        .unwrap()
        .contains("not a real record"));
}

#[test]
fn a1111_without_a_parameter_record_is_all_prompt() {
    // No record means there is nothing else the payload could be — reporting it
    // as the prompt cannot be wrong, and reporting nothing would be a loss.
    let m = parse(&chunks(&[("parameters", "just some words")]), None).unwrap();
    assert_eq!(m.positive, Some(PromptText::literal("just some words")));
    assert!(m.params.is_empty());
}

// ---------------------------------------------------------------------------
// The Details rows
// ---------------------------------------------------------------------------

#[test]
fn detail_rows_lead_with_a_heading_and_put_prompts_in_body_rows() {
    use crate::panels::DetailRow;
    let m = comfy(SIMPLE);
    let rows = detail_rows(&m);
    assert!(
        matches!(&rows[0], DetailRow::Section { text, .. } if text == "Generation (ComfyUI)"),
        "first row must be the section heading, got {:?}",
        rows[0]
    );
    // A prompt is a wrapped paragraph, never a label/value pair — the pair
    // renderer clips to a fixed label column.
    assert!(
        rows.iter()
            .any(|r| matches!(r, DetailRow::Body { text } if text == "a red bird")),
        "the positive prompt must be a Body row: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| matches!(r, DetailRow::Body { text } if text == "blurry")),
        "the negative prompt must be a Body row: {rows:?}"
    );
}

/// Every prompt paragraph is introduced by a **bold** heading.
///
/// Regression (owner-reported, 2026-08-04): "Negative prompt" shipped as
/// `bold: false`, which the presenters treat as a *sub-header* — the
/// regular-weight style the folder path under a filename uses — so it read as
/// stray body text rather than a label for the paragraph beneath it. The bold
/// flag is the only thing distinguishing the two, so it needs pinning.
#[test]
fn every_prompt_paragraph_has_a_bold_heading_above_it() {
    use crate::panels::DetailRow;
    let rows = detail_rows(&comfy(SIMPLE));
    for (i, row) in rows.iter().enumerate() {
        if matches!(row, DetailRow::Body { .. }) {
            let above = i.checked_sub(1).map(|j| &rows[j]);
            assert!(
                matches!(
                    above,
                    Some(DetailRow::Span { bold: true, .. }) | Some(DetailRow::Section { .. })
                ),
                "a Body row must follow a bold heading, got {above:?} above {row:?}"
            );
        }
    }
    // And both are named, so neither paragraph is left to be guessed at.
    let headings: Vec<&str> = rows
        .iter()
        .filter_map(|r| match r {
            DetailRow::Span { text, bold: true } | DetailRow::Section { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        headings,
        ["Generation (ComfyUI)", "Prompt", "Negative prompt"]
    );
}

/// The section heading carries its copy buttons, and **Copy prompt appears only
/// when there is a prompt to copy** — a button whose only outcome is a refusal
/// toast is worse than no button, especially with the reason already on screen
/// directly beneath it.
#[test]
fn the_section_heading_offers_copy_buttons_that_can_actually_succeed() {
    use crate::action::Action;
    use crate::panels::DetailRow;

    let buttons = |m: &GenerationMeta| match &detail_rows(m)[0] {
        DetailRow::Section { actions, .. } => {
            actions.iter().map(|a| a.action).collect::<Vec<Action>>()
        }
        other => panic!("expected a Section heading, got {other:?}"),
    };

    // A literal prompt: both buttons.
    assert_eq!(
        buttons(&comfy(SIMPLE)),
        [Action::CopyGenerationPrompt, Action::CopyGenerationData]
    );

    // An unresolved prompt: data only — the prompt button could only ever fail.
    let unresolved = r#"{
      "1": {"class_type": "PromptCombinator", "inputs": {"input_list_1": "girl"}},
      "2": {"class_type": "CLIPTextEncode", "inputs": {"text": ["1", 0]}},
      "3": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "4": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
            "positive": ["2", 0], "negative": ["2", 0], "latent_image": ["3", 0]}},
      "5": {"class_type": "SaveImage", "inputs": {"images": ["4", 0]}}
    }"#;
    assert_eq!(buttons(&comfy(unresolved)), [Action::CopyGenerationData]);

    // A payload with no readable facts still offers the data copy — that is the
    // whole reason its "not readable" row points at a button.
    let payload_only = parse(&chunks(&[("workflow", r#"{"nodes": []}"#)]), None).unwrap();
    assert_eq!(buttons(&payload_only), [Action::CopyGenerationData]);
}

/// The payload button names what the payload **actually is**. ComfyUI always
/// copies a `workflow`/`prompt` graph, which is JSON; Automatic1111 copies a flat
/// `parameters` block, which is not. One shared word would be wrong on one of
/// them, and the button is built per file, so it can be exact.
#[test]
fn the_payload_button_is_named_for_the_payloads_actual_format() {
    use crate::panels::DetailRow;
    let label = |m: &GenerationMeta| match &detail_rows(m)[0] {
        DetailRow::Section { actions, .. } => actions.last().unwrap().label.clone(),
        other => panic!("expected a Section heading, got {other:?}"),
    };
    assert_eq!(label(&comfy(SIMPLE)), "Copy JSON");

    let a1111 = parse(
        &chunks(&[("parameters", "a cat\nSteps: 20, Seed: 1")]),
        None,
    )
    .unwrap();
    assert_eq!(label(&a1111), "Copy parameters");
}

#[test]
fn an_unresolved_prompt_still_gets_a_row_saying_why() {
    use crate::panels::DetailRow;
    let json = r#"{
      "1": {"class_type": "PromptCombinator", "inputs": {"input_list_1": "girl"}},
      "2": {"class_type": "CLIPTextEncode", "inputs": {"text": ["1", 0]}},
      "3": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
      "4": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
            "positive": ["2", 0], "negative": ["2", 0], "latent_image": ["3", 0]}},
      "5": {"class_type": "SaveImage", "inputs": {"images": ["4", 0]}}
    }"#;
    let rows = detail_rows(&comfy(json));
    // Blank space reads as a bug; naming the cause is the useful answer. It is a
    // Note rather than a Pair so it cannot be read — or pasted — as the prompt.
    assert!(
        rows.iter().any(|r| matches!(r,
            DetailRow::Note { label, text }
                if label == "Prompt" && text.contains("PromptCombinator"))),
        "an unresolved prompt must explain itself as a note: {rows:?}"
    );
    // And the combinator's word list is still nowhere in the output.
    assert!(!format!("{rows:?}").contains("girl"));
}

#[test]
fn an_unreadable_payload_says_so_rather_than_showing_nothing() {
    use crate::panels::DetailRow;
    let m = parse(&chunks(&[("workflow", r#"{"nodes": []}"#)]), None).unwrap();
    let rows = detail_rows(&m);
    assert!(
        rows.iter().any(|r| matches!(r,
            DetailRow::Note { text, .. } if text.contains("button above"))),
        "an unreadable payload must point at the copy button: {rows:?}"
    );
}

#[test]
fn detail_rows_are_empty_for_nothing() {
    // The common case: an ordinary photo grows no rows at all.
    assert!(parse(&chunks(&[("Software", "gimp")]), None).is_none());
}

#[test]
fn an_a1111_shaped_user_comment_is_recognized() {
    // The JPEG/WebP path: the same block arrives via EXIF UserComment.
    let text = "a cat\nNegative prompt: dog\nSteps: 25, Seed: 99";
    let m = parse(&[], Some(text)).unwrap();
    assert_eq!(m.tool, GenTool::Automatic1111);
    assert_eq!(param(&m, "Seed").as_deref(), Some("99"));
    // A caption that merely mentions steps is not a parameters block.
    assert!(parse(&[], Some("Steps I took today: many")).is_none());
}
