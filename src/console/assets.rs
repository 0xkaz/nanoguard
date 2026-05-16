//! Embedded static assets for the nanoguard console.
//!
//! All assets are baked into the binary with `include_str!` so the console
//! ships as a single executable with no external file dependencies.

pub const INDEX_HTML: &str = include_str!("assets/index.html");
pub const STYLES_CSS: &str = include_str!("assets/styles.css");
pub const APP_JS: &str = include_str!("assets/app.js");
