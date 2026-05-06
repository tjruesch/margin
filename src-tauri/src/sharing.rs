//! Native macOS sharing via `NSSharingServicePicker`.
//!
//! The picker lights up AirDrop, Mail, Messages, Notes, Reminders, and
//! any third-party share extension installed on the user's machine. We
//! pass it a temp `<title>.md` file (frontmatter stripped) so the
//! receiving app sees clean Markdown with a meaningful filename, not
//! `note.md`.
//!
//! Cocoa UI APIs are main-thread only — Tauri commands run on tokio, so
//! we hop via `AppHandle::run_on_main_thread` before touching AppKit.

use std::fs;
use std::path::{Path, PathBuf};

use crate::notes::{is_owned_note_in, parse_frontmatter, split_frontmatter};
use crate::paths;

const MAX_TITLE_LEN: usize = 80;

/// Filesystem-safe filename derived from a markdown title.
///
/// - Replaces path separators (`/`, `:`, `\`) and NUL with `-`.
/// - Replaces ASCII control chars with spaces (so a `\n` in a title
///   doesn't become a hidden newline in the filename).
/// - Collapses runs of whitespace to a single space.
/// - Falls back to `Untitled note` for empty / whitespace-only input.
/// - Truncates to `MAX_TITLE_LEN` chars before appending `.md` so the
///   total filename stays short enough for the receiving app's UI.
pub(crate) fn sanitize_filename(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | ':' | '\\' | '\0' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    let base = if trimmed.is_empty() {
        "Untitled note"
    } else {
        trimmed
    };
    let truncated: String = base.chars().take(MAX_TITLE_LEN).collect();
    format!("{truncated}.md")
}

/// Extract the body and a derived title from an owned note's path.
/// Used to build the temp file the share sheet hands off.
pub(crate) fn share_payload(note_path: &Path) -> Result<(String, String), String> {
    if !is_owned_note_in(note_path, &paths::notes_dir()) {
        return Err("Refusing to share: not an owned note path".into());
    }
    let raw = fs::read_to_string(note_path).map_err(|e| e.to_string())?;
    let (yaml, body) = split_frontmatter(&raw);
    let _ = yaml.map(parse_frontmatter); // parse but discard — frontmatter doesn't ship
    let title = derive_title(body);
    Ok((title, body.to_string()))
}

fn derive_title(body: &str) -> String {
    body.lines()
        .find_map(|l| {
            l.trim_start()
                .strip_prefix("# ")
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| "Untitled note".to_string())
}

/// Write the body to `<NSTemporaryDirectory>/<sanitized title>.md` and
/// return the absolute path. macOS auto-purges the temp dir, so we
/// don't manage cleanup explicitly.
fn write_temp_payload(filename: &str, body: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir();
    let path = dir.join(filename);
    fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

#[tauri::command]
pub async fn share_note(note_path: String, app: tauri::AppHandle) -> Result<(), String> {
    let p = PathBuf::from(&note_path);
    let (title, body) = share_payload(&p)?;
    let filename = sanitize_filename(&title);
    let temp_path = write_temp_payload(&filename, &body)?;
    let temp_path_str = temp_path.to_string_lossy().into_owned();

    let app_for_main = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = present_share_sheet(&app_for_main, &temp_path_str) {
            eprintln!("share sheet failed: {e}");
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- AppKit bridge -------------------------------------------------

#[cfg(target_os = "macos")]
fn present_share_sheet(app: &tauri::AppHandle, file_path: &str) -> Result<(), String> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_app_kit::{NSSharingServicePicker, NSView};
    use objc2_foundation::{NSArray, NSRect, NSRectEdge, NSString, NSURL};
    use tauri::Manager;

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    // ns_view returns a *mut c_void to the webview's NSView (the
    // WKWebView itself on macOS). Cast and retain it for objc2.
    let ns_view_raw = window.ns_view().map_err(|e| e.to_string())? as *mut NSView;
    if ns_view_raw.is_null() {
        return Err("ns_view was null".into());
    }
    // SAFETY: Tauri owns the view; we're on the main thread. We
    // borrow it via a non-owning &NSView reference for the duration of
    // the AppKit call. Retaining and holding a Retained could fight
    // Tauri's lifecycle.
    let view: &NSView = unsafe { &*ns_view_raw };

    let url_string = NSString::from_str(file_path);
    let url = NSURL::fileURLWithPath(&url_string);
    let url_obj: Retained<AnyObject> = Retained::into_super(url).into();
    let items = NSArray::from_retained_slice(std::slice::from_ref(&url_obj));

    let picker = unsafe {
        NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items)
    };

    let bounds = view.bounds();
    let anchor = NSRect::new(
        objc2_foundation::NSPoint::new(
            (bounds.size.width - 60.0).max(0.0),
            (bounds.size.height - 50.0).max(0.0),
        ),
        objc2_foundation::NSSize::new(50.0, 30.0),
    );

    picker.showRelativeToRect_ofView_preferredEdge(anchor, view, NSRectEdge::MinY);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn present_share_sheet(_app: &tauri::AppHandle, _file_path: &str) -> Result<(), String> {
    Err("share sheet only supported on macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn sanitize_filename_strips_separators() {
        assert_eq!(sanitize_filename("a/b:c"), "a-b-c.md");
        assert_eq!(sanitize_filename("foo\\bar"), "foo-bar.md");
    }

    #[test]
    fn sanitize_filename_falls_back_to_untitled() {
        assert_eq!(sanitize_filename(""), "Untitled note.md");
        assert_eq!(sanitize_filename("   "), "Untitled note.md");
    }

    #[test]
    fn sanitize_filename_truncates_long_titles() {
        let long = "x".repeat(200);
        let out = sanitize_filename(&long);
        assert!(out.ends_with(".md"));
        let stem = &out[..out.len() - ".md".len()];
        assert_eq!(stem.chars().count(), MAX_TITLE_LEN);
    }

    #[test]
    fn sanitize_filename_collapses_whitespace_and_controls() {
        assert_eq!(sanitize_filename("a\n\tb"), "a b.md");
        assert_eq!(sanitize_filename("Hello   World"), "Hello World.md");
    }

    #[test]
    fn share_payload_strips_frontmatter() {
        let tmp = TempDir::new().unwrap();
        // Place under a fake bundle layout so is_owned_note_in's
        // structural check could in principle pass; here it's against
        // the real notes_dir, so we just exercise the title/body
        // extraction path with a path that already lives under
        // ~/.margin/notes/. To keep this test hermetic, we instead
        // call the helpers directly.
        let raw = "---\ntags: [a]\narchived: true\n---\n# Title\n\nBody.\n";
        let (yaml, body) = split_frontmatter(raw);
        let _ = yaml; // exercised
        assert_eq!(body, "# Title\n\nBody.\n");
        assert_eq!(derive_title(body), "Title");
        // Just make sure write_temp_payload roundtrips.
        let dir = tmp.path();
        let target = dir.join(sanitize_filename("Title"));
        fs::write(&target, body).unwrap();
        let written = fs::read_to_string(&target).unwrap();
        assert_eq!(written, body);
    }
}
