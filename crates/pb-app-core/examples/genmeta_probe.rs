//! Probe the generation-metadata pipeline against real files (task #137).
//!
//! The unit tests exercise hand-authored graph *shapes*; this exercises the
//! whole chain — `read_png_text` → `genmeta::parse` → the facts a panel would
//! show — against files on disk, which the committed suite cannot do.
//!
//! ```sh
//! cargo run -p pb-app-core --example genmeta_probe -- path\to\*.png
//! ```

use pb_app_core::genmeta;

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("{path}: cannot read");
            continue;
        };
        println!("\n=== {path}");
        let chunks = pb_decode::read_png_text(&bytes);
        let user_comment = pb_decode::read_exif_user_comment(&bytes);
        for (k, v) in &chunks {
            println!("  chunk [{k}] {} bytes", v.len());
        }
        let Some(m) = genmeta::parse(&chunks, user_comment.as_deref()) else {
            println!("  (no generation metadata)");
            continue;
        };
        println!("  tool     {}", m.tool.name());
        let show = |label: &str, p: &Option<genmeta::PromptText>| match p {
            None => println!("  {label:<10}(none)"),
            Some(p) => match (&p.text, p.unresolved_reason()) {
                (Some(t), _) => println!("  {label:<10}{t}"),
                (None, Some(why)) => println!("  {label:<10}⚠ {why}"),
                (None, None) => println!("  {label:<10}(empty)"),
            },
        };
        show("positive", &m.positive);
        show("negative", &m.negative);
        println!("  model    {}", m.model.as_deref().unwrap_or("(unknown)"));
        for (name, strength) in &m.loras {
            println!("  lora     {name} {strength}");
        }
        for (label, value) in &m.params {
            println!("  {label:<10}{value}");
        }
        for (i, pass) in m.passes.iter().enumerate() {
            println!("  pass {}   {pass}", i + 2);
        }
        println!("  facts: {}  payload: {}", m.has_facts(), m.has_payload);
    }
}
