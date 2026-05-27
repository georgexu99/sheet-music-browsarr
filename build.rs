// Tailwind CSS is generated separately (Dockerfile builder step pulls the
// standalone binary and runs it). For local cargo check / cargo build
// without Tailwind present, we make sure `dist/styles.css` exists so the
// `include_str!` in src/routes/public.rs compiles. The stub is overwritten
// by the real Tailwind output when the Docker build runs.

use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new("dist");
    if let Err(e) = fs::create_dir_all(dist) {
        eprintln!("cargo:warning=could not create dist/: {e}");
    }
    let css = dist.join("styles.css");
    if !css.exists() {
        let stub = b"/* Tailwind stub. Replaced by tailwindcss output during the Docker build. */\n";
        if let Err(e) = fs::write(&css, stub) {
            eprintln!("cargo:warning=could not write dist/styles.css stub: {e}");
        }
    }

    // Re-run if the Tailwind source or any template changes — the real
    // tailwind output depends on both.
    println!("cargo:rerun-if-changed=assets/tailwind.css");
    println!("cargo:rerun-if-changed=templates");
    println!("cargo:rerun-if-changed=dist/styles.css");
}
