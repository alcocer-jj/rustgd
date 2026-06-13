//! rustgd-webview: the webview window for HTML content.
//!
//! Stage four: the gallery, delete, clear-all, and export, with the toolbar
//! themed to match the egui plot viewer (egui's stock dark and light visuals,
//! following the system theme). The binary takes a widget directory (argv[1])
//! populated by the R hook, where each widget is a `widget-NNNN/` bundle
//! folder plus a `widget-NNNN.txt` descriptor. It serves a shell page (a
//! toolbar above an iframe) and each widget's files under `/widget-NNNN/`.
//!
//! Toolbar actions come back over the page-to-Rust channel
//! (`window.ipc.postMessage`): `delete:<index>` and `clearall` are file
//! operations done inline; `export:<index>` is routed through the event loop
//! as a user event so the native save dialog opens from a safe context, then
//! the chosen widget's bundle is copied out as a self-contained folder.
//!
//! Lifecycle is unchanged: the directory is owned by this viewer and the R
//! hook, never the graphics device. On window close the binary deletes the
//! bundles and descriptors and writes `viewer_closed`. If the directory
//! disappears (R exited and its finalizer swept it), the binary exits.
//!
//! Build:  cargo build --release --bin rustgd-webview --features webview
//! Run:    rustgd-webview /tmp/rustgd-widgets-<pid>

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use wry::{
    http::{header::CONTENT_TYPE, Request, Response},
    WebViewBuilder,
};

const POLL: Duration = Duration::from_millis(200);

/// Events delivered to the event loop from elsewhere (the IPC handler).
enum UserEvent {
    Export(u32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Url,
}

struct Widget {
    kind: Kind,
    entry: Option<String>,
    target: Option<String>,
    title: Option<String>,
}

fn main() -> wry::Result<()> {
    let widgets_dir = match std::env::args().nth(1) {
        Some(arg) => PathBuf::from(arg),
        None => {
            eprintln!("usage: rustgd-webview <widget-directory>");
            std::process::exit(2);
        }
    };
    if !widgets_dir.is_dir() {
        eprintln!(
            "rustgd-webview: widget directory does not exist: {}",
            widgets_dir.display()
        );
        std::process::exit(1);
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("rustgd web")
        .with_inner_size(LogicalSize::new(1000.0, 700.0))
        .build(&event_loop)
        .expect("failed to create window");

    // The protocol handler serves the shell and the widget bundle files.
    let handler_dir = widgets_dir.clone();
    // The IPC handler performs toolbar actions. Delete and clear-all are done
    // inline; export is forwarded to the event loop so the native dialog opens
    // from a safe context rather than from within the WebKit callback.
    let ipc_dir = widgets_dir.clone();
    let ipc_proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    let builder = WebViewBuilder::new()
        .with_custom_protocol("rustgd".into(), move |_webview_id, request| {
            match serve(&handler_dir, request) {
                Ok(response) => response.map(Into::into),
                Err(err) => Response::builder()
                    .header(CONTENT_TYPE, "text/plain")
                    .status(500)
                    .body(err.to_string().into_bytes())
                    .unwrap()
                    .map(Into::into),
            }
        })
        .with_ipc_handler(move |request: Request<String>| {
            handle_ipc(&ipc_dir, &ipc_proxy, request.body().as_str());
        })
        .with_url("rustgd://localhost/");

    let webview = builder.build(&window)?;

    // Poll state lives in the loop closure. last_json is the widget list we
    // last pushed to the shell, so we only push when the set changes.
    let mut last_json: Option<String> = None;
    let mut last_poll: Option<Instant> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);

        let due = last_poll.map(|t| t.elapsed() >= POLL).unwrap_or(true);
        if due {
            last_poll = Some(Instant::now());

            // Directory gone (R exited and its finalizer removed it): exit.
            if !widgets_dir.exists() {
                *control_flow = ControlFlow::Exit;
                return;
            }

            let json = widgets_json(&widgets_dir);
            if last_json.as_deref() != Some(json.as_str()) {
                let script = format!(
                    "if(window.__rustgd_setWidgets){{window.__rustgd_setWidgets({});}}",
                    json
                );
                let _ = webview.evaluate_script(&script);
                last_json = Some(json);
            }
        }

        match event {
            Event::UserEvent(UserEvent::Export(index)) => {
                export_widget(&widgets_dir, index);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Clean this viewer's channel, then leave a marker so the R
                // side can tell the window was closed and relaunch next time.
                clear_widgets(&widgets_dir);
                let _ = fs::write(widgets_dir.join("viewer_closed"), b"");
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

/// Handle a message from the toolbar. `delete:<index>` and `clearall` are file
/// operations; `export:<index>` is forwarded to the event loop.
fn handle_ipc(dir: &Path, proxy: &EventLoopProxy<UserEvent>, message: &str) {
    let message = message.trim();
    if message == "clearall" {
        clear_widgets(dir);
    } else if let Some(rest) = message.strip_prefix("delete:") {
        if let Ok(index) = rest.trim().parse::<u32>() {
            delete_widget(dir, index);
        }
    } else if let Some(rest) = message.strip_prefix("export:") {
        if let Ok(index) = rest.trim().parse::<u32>() {
            let _ = proxy.send_event(UserEvent::Export(index));
        }
    }
}

/// Open a native save dialog and copy the widget's bundle to the chosen path
/// as a self-contained folder. A url widget has no bundle and is skipped.
fn export_widget(dir: &Path, index: u32) {
    let bundle = dir.join(format!("widget-{:04}", index));
    if !bundle.is_dir() {
        return;
    }
    if let Some(dest) = rfd::FileDialog::new()
        .set_file_name(format!("rustgd-widget-{:04}", index))
        .save_file()
    {
        let _ = copy_dir(&bundle, &dest);
    }
}

/// Recursively copy a directory tree.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Remove a single widget's descriptor and bundle folder.
fn delete_widget(dir: &Path, index: u32) {
    let _ = fs::remove_file(dir.join(format!("widget-{:04}.txt", index)));
    let _ = fs::remove_dir_all(dir.join(format!("widget-{:04}", index)));
}

/// All widgets in the directory, parsed and sorted ascending by number.
fn widget_list(dir: &Path) -> Vec<(u32, Widget)> {
    let mut out: Vec<(u32, Widget)> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };
            let number = match name
                .strip_prefix("widget-")
                .and_then(|rest| rest.strip_suffix(".txt"))
                .and_then(|digits| digits.parse::<u32>().ok())
            {
                Some(number) => number,
                None => continue,
            };
            if let Some(widget) = parse_descriptor(&path) {
                out.push((number, widget));
            }
        }
    }
    out.sort_by_key(|(number, _)| *number);
    out
}

/// The widget list as a JSON array of `{index, src, title}`.
fn widgets_json(dir: &Path) -> String {
    let items: Vec<String> = widget_list(dir)
        .iter()
        .map(|(index, widget)| {
            let src = match widget.kind {
                Kind::Url => widget.target.clone().unwrap_or_default(),
                Kind::File => {
                    let entry = widget.entry.as_deref().unwrap_or("index.html");
                    format!("rustgd://localhost/widget-{:04}/{}", index, entry)
                }
            };
            let title = widget.title.clone().unwrap_or_default();
            format!(
                "{{\"index\":{},\"src\":\"{}\",\"title\":\"{}\"}}",
                index,
                json_escape(&src),
                json_escape(&title)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Remove every `widget-NNNN.txt` descriptor and `widget-NNNN/` bundle folder.
fn clear_widgets(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if let Some(rest) = name.strip_prefix("widget-") {
            if path.is_dir() && rest.parse::<u32>().is_ok() {
                let _ = fs::remove_dir_all(&path);
            } else if let Some(digits) = rest.strip_suffix(".txt") {
                if digits.parse::<u32>().is_ok() {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

/// Parse a `key=value` widget descriptor.
fn parse_descriptor(path: &Path) -> Option<Widget> {
    let text = fs::read_to_string(path).ok()?;
    let mut kind: Option<Kind> = None;
    let mut entry: Option<String> = None;
    let mut target: Option<String> = None;
    let mut title: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "kind" => {
                kind = match value.trim().to_ascii_lowercase().as_str() {
                    "file" => Some(Kind::File),
                    "url" => Some(Kind::Url),
                    _ => None,
                };
            }
            "entry" => entry = Some(value.trim().to_string()),
            "target" => target = Some(value.trim().to_string()),
            "title" => title = Some(value.trim().to_string()),
            _ => {}
        }
    }

    Some(Widget {
        kind: kind?,
        entry,
        target,
        title,
    })
}

/// Serve a request. The bare root returns the shell; everything else is a file
/// under the widget directory, contained so a path cannot escape it.
fn serve(
    dir: &Path,
    request: Request<Vec<u8>>,
) -> Result<Response<Vec<u8>>, Box<dyn std::error::Error>> {
    let trimmed = request.uri().path().trim_start_matches('/').to_string();

    if trimmed.is_empty() {
        let html = shell_html(&widgets_json(dir));
        return Ok(Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header("Access-Control-Allow-Origin", "*")
            .body(html.into_bytes())?);
    }

    let canon_root = fs::canonicalize(dir)?;
    let resolved = fs::canonicalize(canon_root.join(&trimmed))?;
    if !resolved.starts_with(&canon_root) {
        return Err(format!("request path escapes widget directory: {trimmed}").into());
    }

    let body = fs::read(&resolved)?;
    let mimetype = mime_for(&resolved);
    Ok(Response::builder()
        .header(CONTENT_TYPE, mimetype)
        .header("Access-Control-Allow-Origin", "*")
        .body(body)?)
}

/// The shell page, with the current widget list injected as the initial state.
fn shell_html(json: &str) -> String {
    SHELL_TEMPLATE.replace("__RUSTGD_LIST__", json)
}

const SHELL_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>rustgd web</title>
<style>
  /* Colors mirror egui's stock light visuals (gray values noted). */
  :root {
    --bar-bg: #f8f8f8;     /* panel_fill, gray 248 */
    --bar-border: #bebebe; /* separators, gray 190 */
    --fg: #3c3c3c;         /* button text, gray 60 */
    --fg-muted: #505050;   /* label text, gray 80 */
    --btn-bg: #e6e6e6;     /* button rest, gray 230 */
    --btn-hover: #dcdcdc;  /* button hover, gray 220 */
    --focus: #2a7ef0;
  }
  /* egui's stock dark visuals. */
  @media (prefers-color-scheme: dark) {
    :root {
      --bar-bg: #1b1b1b;     /* panel_fill, gray 27 */
      --bar-border: #3c3c3c; /* separators, gray 60 */
      --fg: #b4b4b4;         /* button text, gray 180 */
      --fg-muted: #8c8c8c;   /* label text, gray 140 */
      --btn-bg: #3c3c3c;     /* button rest, gray 60 */
      --btn-hover: #464646;  /* button hover, gray 70 */
      --focus: #4a93ff;
    }
  }
  html, body { height: 100%; margin: 0; }
  body {
    display: flex;
    flex-direction: column;
    font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    color: var(--fg);
  }
  #bar {
    flex: 0 0 auto;
    height: 44px;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 8px;
    background: var(--bar-bg);
    border-bottom: 1px solid var(--bar-border);
    -webkit-user-select: none;
    user-select: none;
  }
  .btn {
    height: 26px;
    padding: 0 10px;
    border: 0;
    border-radius: 3px;
    background: var(--btn-bg);
    color: var(--fg);
    font: inherit;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn.arrow { font-size: 15px; padding: 0 9px; }
  .btn:hover:not(:disabled) { background: var(--btn-hover); }
  .btn:disabled { opacity: 0.45; cursor: default; }
  .btn:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
  .sep { width: 1px; height: 20px; background: var(--bar-border); margin: 0 4px; }
  #counter {
    color: var(--fg-muted);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    margin: 0 2px;
  }
  .spacer { flex: 1 1 auto; }
  #stage { flex: 1 1 auto; position: relative; background: var(--bar-bg); }
  #frame {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    border: 0;
    background: #fff;
  }
  #empty {
    position: absolute;
    inset: 0;
    display: none;
    align-items: center;
    justify-content: center;
    color: var(--fg-muted);
    background: var(--bar-bg);
  }
</style>
</head>
<body>
  <div id="bar">
    <button id="prev" class="btn arrow" title="Previous (left arrow)">&#9664; Prev</button>
    <button id="next" class="btn arrow" title="Next (right arrow)">Next &#9654;</button>
    <span class="sep"></span>
    <span id="counter">no widgets</span>
    <span class="spacer"></span>
    <button id="export" class="btn" title="Export this widget to a folder">Export...</button>
    <button id="del" class="btn" title="Remove the widget on screen">Clear widget</button>
    <button id="clear" class="btn" title="Remove all widgets">Clear all</button>
  </div>
  <div id="stage">
    <iframe id="frame" title="widget"></iframe>
    <div id="empty">no widgets yet</div>
  </div>
<script>
  (function () {
    window.__RUSTGD_INITIAL = __RUSTGD_LIST__;

    var widgets = window.__RUSTGD_INITIAL || [];
    var pos = widgets.length - 1;
    var currentSrc = null;

    var frame = document.getElementById("frame");
    var empty = document.getElementById("empty");
    var counter = document.getElementById("counter");
    var prevBtn = document.getElementById("prev");
    var nextBtn = document.getElementById("next");
    var exportBtn = document.getElementById("export");
    var delBtn = document.getElementById("del");
    var clearBtn = document.getElementById("clear");

    function send(message) {
      if (window.ipc && window.ipc.postMessage) window.ipc.postMessage(message);
    }

    function render() {
      var n = widgets.length;
      if (n === 0) {
        counter.textContent = "no widgets";
        prevBtn.disabled = true;
        nextBtn.disabled = true;
        exportBtn.disabled = true;
        delBtn.disabled = true;
        clearBtn.disabled = true;
        empty.style.display = "flex";
        if (currentSrc !== "") { frame.src = "about:blank"; currentSrc = ""; }
        return;
      }
      empty.style.display = "none";
      if (pos < 0) pos = 0;
      if (pos > n - 1) pos = n - 1;
      counter.textContent = "Widget " + (pos + 1) + " / " + n;
      prevBtn.disabled = (pos === 0);
      nextBtn.disabled = (pos === n - 1);
      exportBtn.disabled = false;
      delBtn.disabled = false;
      clearBtn.disabled = false;
      var src = widgets[pos].src;
      if (src !== currentSrc) { frame.src = src; currentSrc = src; }
    }

    window.__rustgd_setWidgets = function (list) {
      var prevLen = widgets.length;
      widgets = list || [];
      // A widget was added: bring the window to the newest. On a delete or
      // clear the length shrinks, so we keep position and let render clamp.
      if (widgets.length > prevLen) pos = widgets.length - 1;
      render();
    };

    prevBtn.addEventListener("click", function () {
      if (pos > 0) { pos--; render(); }
    });
    nextBtn.addEventListener("click", function () {
      if (pos < widgets.length - 1) { pos++; render(); }
    });
    exportBtn.addEventListener("click", function () {
      if (widgets.length > 0) send("export:" + widgets[pos].index);
    });
    delBtn.addEventListener("click", function () {
      if (widgets.length > 0) send("delete:" + widgets[pos].index);
    });
    clearBtn.addEventListener("click", function () {
      if (widgets.length > 0) send("clearall");
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "ArrowLeft" && pos > 0) { pos--; render(); }
      else if (e.key === "ArrowRight" && pos < widgets.length - 1) { pos++; render(); }
    });

    render();
  })();
</script>
</body>
</html>
"##;

fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
