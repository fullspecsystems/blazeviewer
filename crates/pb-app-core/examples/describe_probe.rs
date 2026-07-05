//! Dev probe for the AI image-description **endpoint** backend (task #44, subtask 4) —
//! the `image_text_probe` precedent: run the exact worker job the app will run (full-res
//! decode → tone-map → downscale + JPEG → POST to an OpenAI-compatible endpoint) against
//! one file and print the description. Exercises the real HTTP path + a live vision model,
//! which the unit tests deliberately don't.
//!
//! Point it at a local server (LM Studio / llama.cpp / Ollama with a vision model):
//!
//! ```sh
//! cargo run -p pb-app-core --example describe_probe -- photo.jpg
//! cargo run -p pb-app-core --example describe_probe -- photo.jpg http://localhost:11434/v1 qwen2.5vl
//! cargo run -p pb-app-core --example describe_probe -- photo.jpg http://localhost:1234/v1 "" "What text is in this image?"
//! ```
//!
//! The prompt (when not overridden by the 4th arg) is built from the file's own EXIF via
//! the subtask-2 prompt builder, so this smokes subtasks 2 + 4 together.

use std::time::Instant;

use pb_app_core::describe::{describe_job, LocalEndpoint};
use pb_app_core::prompt::{build_context, build_prompt};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: describe_probe <image> [endpoint] [model] [prompt]");
        std::process::exit(2);
    };
    let endpoint = args.next().unwrap_or_else(|| "http://localhost:1234/v1".to_string());
    let model = args.next().unwrap_or_default();
    let custom_prompt = args.next();

    // Build the same prompt the app would: salient EXIF + filename/folder → instruction.
    let prompt = match &custom_prompt {
        Some(p) => p.clone(),
        None => {
            let exif = std::fs::read(&path)
                .map(|b| pb_decode::read_exif_fields(&b))
                .unwrap_or_default();
            let ctx = build_context(&path, &exif, None);
            build_prompt(&ctx, None)
        }
    };

    println!("endpoint: {endpoint}");
    println!("model:    {}", if model.is_empty() { "(server default)" } else { &model });
    println!("prompt:\n{prompt}\n---");

    let source = pb_source::FsSource::new(vec![path.clone().into()]);
    let backend = LocalEndpoint::new(endpoint, model, 512);
    let t0 = Instant::now();
    match describe_job(&source, 0, pb_render::Rotation::default(), &prompt, &backend) {
        Ok(text) => println!("described in {:?}:\n\n{text}", t0.elapsed()),
        Err(e) => {
            eprintln!("describe failed after {:?}: {}", t0.elapsed(), e.user_message());
            std::process::exit(1);
        }
    }
}
