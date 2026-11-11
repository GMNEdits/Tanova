//! Headless Tanova entry point. Scaffolding only.

fn main() {
    tanova_nodes::ensure_linked();
    println!("tanova-cli {} (scaffolding)", env!("CARGO_PKG_VERSION"));
}
