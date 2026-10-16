pub mod context;
pub mod display;

pub use context::{RustEngine, RustEngineError, RustExecutionOutput};
pub use display::{format_mime_bundle, EVCXR_STARTUP_PRELUDE};

/// Re-export `evcxr::runtime_hook` so the main binary can invoke it at the top of `main()`.
pub fn runtime_hook() {
    evcxr::runtime_hook();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_svg_and_html_mime_promotion() {
        let mut raw = HashMap::new();
        raw.insert(
            "text/plain".to_string(),
            r#""<svg width=\"100\" height=\"100\"><circle cx=\"50\" cy=\"50\" r=\"40\"/></svg>""#
                .to_string(),
        );
        let (data, _) = format_mime_bundle(&raw);
        assert!(data.contains_key("image/svg+xml"));

        let mut raw_html = HashMap::new();
        raw_html.insert(
            "text/plain".to_string(),
            r#""<table><tr><td>42</td></tr></table>""#.to_string(),
        );
        let (data_html, _) = format_mime_bundle(&raw_html);
        assert!(data_html.contains_key("text/html"));
    }
}
