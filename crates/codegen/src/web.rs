//! What a program built for web pages comes with: the JavaScript glue that
//! loads its module, gives it the host functions the runtime imports from
//! `sayon` (output, clocks, random bytes, exit, options; see the runtime's
//! `console`), and runs it on the page's event loop; and a page that runs
//! it and shows its output. The user never writes either.

/// What the module exports for the glue, besides its memory.
pub const EXPORTS: &[&str] = &["sayon_run", "sayon_wait_milliseconds"];

const GLUE: &str = include_str!("web/glue.js");
const PAGE: &str = include_str!("web/page.html");

/// The glue for the module file named `module`, which lies beside it.
pub fn glue(module: &str) -> String {
    GLUE.replace("__MODULE__", &escape_js(module))
}

/// The page that runs the program through the glue file named `glue`.
pub fn page(title: &str, glue: &str) -> String {
    PAGE.replace("__TITLE__", &escape_html(title))
        .replace("__GLUE__", &escape_js(glue))
}

fn escape_js(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
