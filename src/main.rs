use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use lsp_server::{Connection, Message, Response};
use lsp_types::{
    notification::{DidChangeTextDocument, DidOpenTextDocument, Notification as _},
    request::{Completion, Request as _},
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams,
    CompletionResponse, CompletionTextEdit, InitializeParams, Position, Range,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
};
use notify::{RecursiveMode, Watcher};
use walkdir::WalkDir;

type RouteIndex = Arc<Mutex<Vec<String>>>;
type Docs = Arc<Mutex<HashMap<Url, String>>>;

fn main() -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let server_capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["\"".into(), "'".into(), "/".into()]),
            resolve_provider: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    })?;

    let initialize_value = connection.initialize(server_capabilities)?;
    let initialize_params: InitializeParams = serde_json::from_value(initialize_value)?;

    let root: PathBuf = initialize_params
        .root_uri
        .as_ref()
        .and_then(|u| u.to_file_path().ok())
        .or_else(|| {
            initialize_params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first())
                .and_then(|f| f.uri.to_file_path().ok())
        })
        .unwrap_or_else(|| PathBuf::from("."));

    let routes_dir = root.join("src").join("routes");

    let routes: RouteIndex = Arc::new(Mutex::new(scan_routes(&routes_dir)));
    let docs: Docs = Arc::new(Mutex::new(HashMap::new()));

    // Watch src/routes and rebuild the route index whenever it changes,
    // so newly added/renamed pages show up without restarting the server.
    let watch_routes = routes.clone();
    let watch_dir = routes_dir.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            if let Ok(mut guard) = watch_routes.lock() {
                *guard = scan_routes(&watch_dir);
            }
        }
    })?;
    if routes_dir.is_dir() {
        let _ = watcher.watch(&routes_dir, RecursiveMode::Recursive);
    }

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }
                if req.method == Completion::METHOD {
                    let id = req.id.clone();
                    let params: CompletionParams = serde_json::from_value(req.params)?;
                    let response = handle_completion(params, &docs, &routes);
                    connection.sender.send(Message::Response(Response {
                        id,
                        result: Some(serde_json::to_value(response)?),
                        error: None,
                    }))?;
                }
            }
            Message::Notification(not) => match not.method.as_str() {
                m if m == DidOpenTextDocument::METHOD => {
                    let p: lsp_types::DidOpenTextDocumentParams =
                        serde_json::from_value(not.params)?;
                    docs.lock()
                        .unwrap()
                        .insert(p.text_document.uri, p.text_document.text);
                }
                m if m == DidChangeTextDocument::METHOD => {
                    let p: lsp_types::DidChangeTextDocumentParams =
                        serde_json::from_value(not.params)?;
                    // We declared full-document sync, so the last change
                    // event contains the complete new text.
                    if let Some(change) = p.content_changes.into_iter().last() {
                        docs.lock().unwrap().insert(p.text_document.uri, change.text);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    io_threads.join()?;
    Ok(())
}

/// Walks `src/routes` and produces a flat list of routable URL paths,
/// following Qwik City's file-based routing conventions:
/// - directories become path segments
/// - `(group)` directories are layout-only and excluded from the URL
/// - `[param]` / `[...rest]` segments are kept as-is, so the developer can
///   see at a glance what still needs to be filled in
/// - a directory only becomes a suggestion if it actually contains an
///   `index.*` file (i.e. it is a real page, not just an intermediate
///   folder)
fn scan_routes(routes_dir: &Path) -> Vec<String> {
    if !routes_dir.is_dir() {
        return Vec::new();
    }

    let mut routes = Vec::new();

    for entry in WalkDir::new(routes_dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !file_name.starts_with("index.") {
            continue;
        }

        let parent = entry.path().parent().unwrap_or(routes_dir);
        let rel = parent.strip_prefix(routes_dir).unwrap_or(parent);

        let mut segments = Vec::new();
        for part in rel.components() {
            let part = part.as_os_str().to_string_lossy();
            if part.starts_with('(') && part.ends_with(')') {
                continue; // route group: layout-only, not part of the URL
            }
            segments.push(part.to_string());
        }

        let mut path = String::from("/");
        path.push_str(&segments.join("/"));
        if !path.ends_with('/') {
            path.push('/');
        }
        routes.push(path);
    }

    routes.sort();
    routes.dedup();
    routes
}

fn handle_completion(
    params: CompletionParams,
    docs: &Docs,
    routes: &RouteIndex,
) -> Option<CompletionResponse> {
    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let docs_guard = docs.lock().ok()?;
    let text = docs_guard.get(&uri)?;
    let offset = position_to_offset(text, position)?;

    let (value_start, value_end) = find_link_href_value_range(text, offset)?;
    let typed = &text[value_start..offset.min(value_end)];

    let routes_guard = routes.lock().ok()?;
    let items: Vec<CompletionItem> = routes_guard
        .iter()
        .filter(|route| route.starts_with(typed))
        .map(|route| {
            let start = offset_to_position(text, value_start);
            let end = offset_to_position(text, value_end);
            CompletionItem {
                label: route.clone(),
                kind: Some(CompletionItemKind::FILE),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: Range::new(start, end),
                    new_text: route.clone(),
                })),
                ..Default::default()
            }
        })
        .collect();

    Some(CompletionResponse::Array(items))
}

/// Determines whether `offset` sits inside the string value of an `href`
/// attribute on a `<Link ...>` opening tag, and if so returns the byte
/// range of that value (between the quotes) within `text`.
///
/// This is a deliberately lightweight scan rather than a full JSX parse:
/// it walks backward from the cursor to find the enclosing quoted string,
/// confirms it's the value of `href=`, then confirms the nearest unclosed
/// `<` belongs to a `Link` tag. This covers the common cases (including
/// multi-line attribute lists) without pulling in a full TSX parser.
fn find_link_href_value_range(text: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    let mut quote: Option<(usize, u8)> = None;
    let mut i = offset;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'"' | b'\'' => {
                quote = Some((i, bytes[i]));
                break;
            }
            b'<' | b'>' => return None,
            _ => continue,
        }
    }
    let (quote_idx, quote_char) = quote?;

    let mut end = offset;
    while end < bytes.len() && bytes[end] != quote_char && bytes[end] != b'\n' {
        end += 1;
    }

    let before_quote = &text[..quote_idx];
    let trimmed = before_quote.trim_end();
    if !trimmed.ends_with("href=") && !trimmed.ends_with("href =") {
        return None;
    }
    let attr_start = trimmed.rfind("href")?;

    let head = &text[..attr_start];
    let last_open = head.rfind('<')?;
    if let Some(last_close) = head.rfind('>') {
        if last_close > last_open {
            return None; // the nearest tag was already closed
        }
    }
    let tag_name: String = head[last_open + 1..]
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .collect();
    if tag_name != "Link" {
        return None;
    }

    Some((quote_idx + 1, end))
}

fn position_to_offset(text: &str, pos: Position) -> Option<usize> {
    let mut offset = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i as u32 == pos.line {
            let char_offset: usize = line
                .chars()
                .take(pos.character as usize)
                .map(|c| c.len_utf8())
                .sum();
            return Some(offset + char_offset);
        }
        offset += line.len() + 1;
    }
    None
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut last_line_start = 0usize;
    for (i, b) in text.as_bytes().iter().enumerate() {
        if i >= offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
            last_line_start = i + 1;
        }
    }
    let character = text[last_line_start..offset].chars().count() as u32;
    Position::new(line, character)
}