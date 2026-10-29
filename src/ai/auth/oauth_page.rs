//! The browser page served by the loopback OAuth callback server.
//!
//! Rust equivalent of upstream `packages/ai/src/utils/oauth-page.ts`,
//! including the pi logo SVG and dark theme styles.

const LOGO_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 800 800\" aria-hidden=\"true\"><path fill=\"#F09082\" d=\"M165.29 165.29H517.36V400H400V282.65H165.29Z\"/><path fill=\"#4D9ABF\" d=\"M165.29 282.65H282.65V400H400V517.36H282.65V634.72H165.29Z\"/><path fill=\"#F1BE58\" d=\"M517.36 400H634.72V634.72H517.36Z\"/></svg>";

/// Escape text for safe HTML interpolation.
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let title = escape_html(title);
    let heading = escape_html(heading);
    let message = escape_html(message);
    let details = details.map(escape_html);
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\" />\n  \
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  \
         <title>{title}</title>\n  <style>\n    :root {{\n      --text: #fafafa;\n      \
         --text-dim: #a1a1aa;\n      --page-bg: #09090b;\n      --font-sans: ui-sans-serif, system-ui, \
         -apple-system, BlinkMacSystemFont, \"Segoe UI\", Roboto, \"Helvetica Neue\", Arial, \
         \"Noto Sans\", sans-serif, \"Apple Color Emoji\", \"Segoe UI Emoji\", \"Segoe UI Symbol\", \
         \"Noto Color Emoji\";\n      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, \
         Consolas, \"Liberation Mono\", \"Courier New\", monospace;\n    }}\n    \
         * {{ box-sizing: border-box; }}\n    html {{ color-scheme: dark; }}\n    body {{\n      \
         margin: 0;\n      min-height: 100vh;\n      display: flex;\n      align-items: center;\n      \
         justify-content: center;\n      padding: 24px;\n      background: var(--page-bg);\n      \
         color: var(--text);\n      font-family: var(--font-sans);\n      text-align: center;\n    }}\n    \
         main {{\n      width: 100%;\n      max-width: 560px;\n      display: flex;\n      \
         flex-direction: column;\n      align-items: center;\n      justify-content: center;\n    }}\n    \
         .logo {{\n      width: 72px;\n      height: 72px;\n      display: block;\n      \
         margin-bottom: 24px;\n    }}\n    h1 {{\n      margin: 0 0 10px;\n      font-size: 28px;\n      \
         line-height: 1.15;\n      font-weight: 650;\n      color: var(--text);\n    }}\n    p {{\n      \
         margin: 0;\n      line-height: 1.7;\n      color: var(--text-dim);\n      font-size: 15px;\n    }}\n    \
         .details {{\n      margin-top: 16px;\n      font-family: var(--font-mono);\n      \
         font-size: 13px;\n      color: var(--text-dim);\n      white-space: pre-wrap;\n      \
         word-break: break-word;\n    }}\n  </style>\n</head>\n<body>\n  <main>\n    \
         <div class=\"logo\">{LOGO_SVG}</div>\n    <h1>{heading}</h1>\n    <p>{message}</p>\n{details_html}  \
         </main>\n</body>\n</html>",
        title = title,
        heading = heading,
        message = message,
        LOGO_SVG = LOGO_SVG,
        details_html = match &details {
            Some(details) => format!("    <div class=\"details\">{details}</div>\n"),
            None => String::new(),
        },
    )
}

/// The success page (upstream `oauthSuccessHtml`).
pub fn oauth_success_html(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

/// The failure page (upstream `oauthErrorHtml`).
pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}
