//! Server-rendered monitor pages (ADR 0010).
//!
//! Templates are inlined as Askama `source` so M6 needs no `templates/`
//! directory; M7 can move them to files as the UI grows. Askama HTML-escapes all
//! interpolated values, so error text and the CSRF token are safe to render.

use askama::Template;

/// The login page. `error` is empty when there is nothing to report.
#[derive(Template)]
#[template(
    source = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Telegraph Monitor — Sign in</title>
</head>
<body>
<h1>Telegraph Monitor</h1>
{% if !error.is_empty() %}<p role="alert">{{ error }}</p>{% endif %}
<form method="post" action="/monitor/login">
<label>Username <input type="text" name="username" autocomplete="username"></label>
<label>Password <input type="password" name="password" autocomplete="current-password"></label>
<button type="submit">Sign in</button>
</form>
</body>
</html>
"#,
    ext = "html"
)]
pub struct LoginTemplate {
    /// A generic error message to show, or empty for none.
    pub error: String,
}

/// The authenticated monitor placeholder. M7 replaces this with the Recording
/// views; for now it confirms the session and offers logout.
#[derive(Template)]
#[template(
    source = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Telegraph Monitor</title>
</head>
<body>
<h1>Monitor</h1>
<p>Signed in as {{ username }}.</p>
<p>Monitor placeholder. Recording views arrive in M7.</p>
<form method="post" action="/monitor/logout">
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
<button type="submit">Log out</button>
</form>
</body>
</html>
"#,
    ext = "html"
)]
pub struct MonitorTemplate {
    /// The signed-in Operator's username.
    pub username: String,
    /// The raw CSRF token to embed in the logout form.
    pub csrf_token: String,
}
