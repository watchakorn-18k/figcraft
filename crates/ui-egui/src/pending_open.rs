//! Files dropped on the home screen or the canvas open a frame later, behind a card that says what
//! is happening: opening a large `.fig` blocks the UI thread for a while, and the card is on screen
//! before it does (rather than a window that freezes without a word). A file with no path to read
//! (every drop in the browser) waits behind the card until its contents arrive.

use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

use figcraft_doc::NodeId;
use figcraft_engine::cmd::file::Preloaded;
use serde_json::{Value, json};

use crate::i18n::{tr, trf, trn};
use crate::theme::{Tokens, semibold};
use crate::{FigApp, ReadFuture, RepaintWaker};

/// A dropped file waiting to be opened (`file.open`, a document of its own) or imported
/// (`file.import`, into the current document).
pub struct PendingOpen {
    /// What the card shows for it (the file name).
    pub label: String,
    pub params: Value,
    pub command: &'static str,
    /// Canvas imports stay with the document and page that received the drop.
    pub target: Option<ImportTarget>,
    /// The file's contents still on their way; they go into `params` as `data` once read.
    pub read: Option<ReadFuture>,
    /// A file on disk being read and decoded on a thread of its own, so the window keeps drawing
    /// while a large one opens; `file.open` then takes the result instead of reading the file.
    pub load: Option<LoadState>,
}

pub enum LoadState {
    Running(std::sync::mpsc::Receiver<Result<Preloaded, String>>),
    Done(Box<Preloaded>),
}

/// Files this size and up load on a thread of their own ([`loads_in_background`]).
const BACKGROUND_LOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Whether the file at `path` is slow enough to open that it loads on a thread of its own: a `.fig`
/// (decoded and rebuilt layer by layer) or any file of 8 MiB and up. Never in the browser, which
/// has no threads to spare.
pub fn loads_in_background(path: &str) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let p = std::path::Path::new(path);
        p.extension().is_some_and(|e| e.eq_ignore_ascii_case("fig")) || std::fs::metadata(p).is_ok_and(|m| m.len() >= BACKGROUND_LOAD_BYTES)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        false
    }
}

/// Starts loading `path` on a thread; `None` where there are no threads or one couldn't start.
fn spawn_load(path: &str) -> Option<LoadState> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.to_string();
        std::thread::Builder::new()
            .name("figcraft-open".into())
            .spawn(move || {
                // A closed receiver means the user cancelled: the result is dropped.
                let _ = tx.send(figcraft_engine::cmd::file::preload(&path).map_err(|e| e.to_string()));
            })
            .ok()?;
        Some(LoadState::Running(rx))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

/// Queues the file at `path` to open as a document of its own, loading it on a thread first.
/// Callers checked [`loads_in_background`]; a file that can't be loaded that way opens at once.
pub fn open_later(app: &mut FigApp, path: &str) {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let params = json!({"path": path});
    match spawn_load(path) {
        Some(load) => app.ui.pending_opens.push(PendingOpen { label: name, params, command: "file.open", target: None, read: None, load: Some(load) }),
        None => {
            if app.run_user("file.open", params).is_some() {
                app.ui.home = false;
            }
        }
    }
}

/// `file.open` for `params` (from the Open dialog or the Home screen): at once, or behind the
/// card with the file loading on a thread when it is a large one.
pub(crate) fn open_user(app: &mut FigApp, params: Value) {
    let plain = params.get("data").is_none() && params.get("variables").is_none();
    let slow = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| plain && loads_in_background(p) && !app.session.docs.iter().any(|d| d.path.as_deref() == Some(*p)));
    if let Some(path) = slow.map(str::to_string) {
        open_later(app, &path);
    } else if app.run_user("file.open", params).is_some() {
        app.ui.home = false;
    }
}

pub struct ImportTarget {
    pub uid: u64,
    pub page: NodeId,
    pub name: String,
}

/// Queues dropped file `f` to open as a document of its own: by path, or by its contents when there
/// is no file on disk to read. Callers checked its extension, so there is a name to tell the format by.
pub(crate) fn open_dropped(app: &mut FigApp, f: &egui::DroppedFileHandle) {
    let path = f.path();
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (params, read) =
        if path.exists() { (json!({"path": path.to_string_lossy()}), None) } else { (json!({"name": name}), Some(crate::dropped_bytes(f, &name))) };
    app.ui.pending_opens.push(PendingOpen { label: name, params, command: "file.open", target: None, read, load: None });
}

fn run_pending(app: &mut FigApp, pending: PendingOpen) -> Option<Value> {
    let target = match pending.target {
        Some(target) => target,
        None if pending.command != "file.import" => return app.run_user(pending.command, pending.params),
        None => {
            cancel(app, tr("The destination document is unavailable. Nothing was imported.").to_string());
            return None;
        }
    };
    let Some(index) = app.session.docs.iter().position(|doc| doc.uid == target.uid) else {
        cancel(app, tr("The destination document was closed. Nothing was imported.").to_string());
        return None;
    };
    if !app.session.docs[index].doc.pages.contains(&target.page) {
        cancel(app, tr("The destination page was removed. Nothing was imported.").to_string());
        return None;
    }

    let previous = app.session.ds().ok().map(|doc| doc.uid);
    let previous_page = app.session.docs[index].page;
    let previous_selection = app.session.docs[index].selection.clone();
    app.session.active = index;
    app.session.docs[index].page = target.page;
    let result = app.run_user(pending.command, pending.params);
    if previous_page != target.page
        && let Some(doc) = app.session.docs.iter_mut().find(|doc| doc.uid == target.uid)
        && doc.doc.pages.contains(&previous_page)
    {
        doc.page = previous_page;
        doc.selection = previous_selection;
        doc.clean_selection();
    }
    if let Some(index) = previous.and_then(|uid| app.session.docs.iter().position(|doc| doc.uid == uid)) {
        app.session.active = index;
    }
    result
}

/// An import that has no destination left: nothing is imported, and the user is told why.
fn cancel(app: &mut FigApp, message: String) {
    app.show_error(message);
}

/// Space between files dropped together on the canvas.
const IMPORT_GAP: f64 = 20.0;

/// Frames the card is drawn before the files open: egui lays a new card out invisibly on its first
/// frame, so the second one is the first the window shows.
const FRAMES_BEFORE_OPENING: u8 = 2;

/// Polls the contents still being read: those that arrived go into their file's parameters, files
/// that couldn't be read are dropped with a message. Whether any are still on their way.
fn read_contents(app: &mut FigApp, ctx: &egui::Context) -> bool {
    let waker = Waker::from(Arc::new(RepaintWaker(ctx.clone())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut reading = false;
    let mut unread = Vec::new();
    app.ui.pending_opens.retain_mut(|pending| {
        let Some(read) = pending.read.as_mut() else { return true };
        match read.as_mut().poll(&mut cx) {
            Poll::Pending => {
                reading = true;
                true
            }
            Poll::Ready(Ok(bytes)) => {
                use base64::Engine as _;
                pending.params["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
                pending.read = None;
                true
            }
            Poll::Ready(Err(why)) => {
                unread.push(why);
                false
            }
        }
    });
    for why in unread {
        app.show_error(why);
    }
    reading
}

/// Polls the files loading on threads: those that finished wait for their turn to open, those
/// that failed are dropped with a message. Whether any are still loading.
fn poll_loads(app: &mut FigApp) -> bool {
    use std::sync::mpsc::TryRecvError;
    let mut loading = false;
    let mut failed = Vec::new();
    app.ui.pending_opens.retain_mut(|pending| {
        let Some(LoadState::Running(rx)) = pending.load.as_ref() else { return true };
        match rx.try_recv() {
            Ok(Ok(loaded)) => {
                pending.load = Some(LoadState::Done(Box::new(loaded)));
                true
            }
            Ok(Err(why)) => {
                failed.push(format!("{}: {why}", pending.label));
                false
            }
            Err(TryRecvError::Empty) => {
                loading = true;
                true
            }
            Err(TryRecvError::Disconnected) => {
                failed.push(format!("{}: {}", pending.label, tr("The file couldn't be opened.")));
                false
            }
        }
    });
    for why in failed {
        app.show_error(why);
    }
    loading
}

/// The card while files wait, then (once it has been on screen and their contents are read) open
/// them. Called before the import report, so a report for these files shows after them.
pub fn pending_opens(app: &mut FigApp, ctx: &egui::Context) {
    let reading = read_contents(app, ctx) | poll_loads(app);
    if app.ui.pending_opens.is_empty() {
        app.ui.pending_open_frames = 0;
        return;
    }
    ctx.request_repaint();
    if app.ui.pending_open_frames >= FRAMES_BEFORE_OPENING && !reading {
        app.ui.pending_open_frames = 0;
        // Files dropped together land side by side: each starts where the previous one ends.
        let mut next_x: Option<f64> = None;
        for mut pending in std::mem::take(&mut app.ui.pending_opens) {
            let command = pending.command;
            let canvas = command == "file.import" && pending.params.get("x").is_some();
            if canvas && let Some(x) = next_x {
                pending.params["x"] = json!(x);
            }
            let page = pending.target.as_ref().map(|t| (t.uid, t.page));
            if let Some(LoadState::Done(loaded)) = pending.load.take() {
                app.session.preloaded = Some(*loaded);
            }
            let result = run_pending(app, pending);
            app.session.preloaded = None;
            if result.is_some() && command == "file.open" {
                app.ui.home = false;
            }
            if canvas {
                let right = result.as_ref().and_then(|r| r.get("id")?.as_u64()).and_then(|id| {
                    let doc = &app.session.docs.iter().find(|d| Some(d.uid) == page.map(|p| p.0))?.doc;
                    doc.world_bounds(NodeId(id)).map(|b| b.x1)
                });
                next_x = right.map(|r| r + IMPORT_GAP).or(next_x);
            }
        }
        return;
    }
    app.ui.pending_open_frames = app.ui.pending_open_frames.saturating_add(1);
    let title = match app.ui.pending_opens.first().map(|p| p.command) {
        Some("file.import") => {
            let name = app
                .ui
                .pending_opens
                .first()
                .and_then(|pending| pending.target.as_ref().map(|target| target.name.clone()))
                .or_else(|| app.session.doc().ok().map(|d| d.name.clone()))
                .unwrap_or_default();
            trf("Adding to {name}", &[("name", &name)])
        }
        _ => trn("Opening {n} files as new files", app.ui.pending_opens.len(), &[]),
    };
    let t = Tokens::get(ctx);
    let pending = &app.ui.pending_opens;
    let loading = pending.iter().any(|p| p.load.is_some());
    let mut cancel = false;
    // Shown at once: the frame after this one may block the window while the file opens.
    crate::dialogs::window("", ctx).id(egui::Id::new("pending_open")).title_bar(false).movable(false).fade_in(false).show(
        ctx,
        |ui| {
            ui.set_width(480.0);
            ui.label(egui::RichText::new(title).font(semibold(13.0)));
            ui.add_space(12.0);
            for p in pending {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.label(p.label.as_str());
                });
            }
            ui.add_space(16.0);
            ui.label(egui::RichText::new(tr("Opening the file…")).color(t.text_secondary));
            if loading {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(tr("Large files can take a minute.")).color(t.text_secondary));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        cancel = ui.button(tr("Cancel")).clicked();
                    });
                });
            }
        },
    );
    if cancel {
        // The threads finish on their own and their results are dropped.
        app.ui.pending_opens.retain(|p| p.load.is_none());
        app.ui.pending_open_frames = 0;
    }
}
