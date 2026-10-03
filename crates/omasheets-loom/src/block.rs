//! Custom `spreadsheet` block leaf composed into the block editor from outside.
//!
//! The kit paints `BlockType::Custom` as a label unless a host registers a
//! composer. Ashlar (and other Loom hosts) call [`install`] with a
//! [`WorkbookPort`] so note compositions can embed a workbook by reference
//! without forking block-view.

use crate::{PortError, WorkbookDraft, WorkbookPort, WorkbookVersion};
use gpui_component_block_view::{BlockSnapshot, register_custom_block};
use gpui_shell::gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Global, IntoElement as _,
    ParentElement as _, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use omasheets_gpui::{SpreadsheetSession, SpreadsheetUiEvent, SpreadsheetView};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// `BlockType::Custom` name for an embedded workbook leaf.
pub const SPREADSHEET_BLOCK: &str = "spreadsheet";

struct BlockPort {
    port: Arc<dyn WorkbookPort>,
    readonly: bool,
}

impl Global for BlockPort {}

struct LeafCache {
    leaves: HashMap<String, Entity<Leaf>>,
}

impl Global for LeafCache {}

/// Register the spreadsheet custom-block composer for this app.
///
/// Safe to call more than once; later calls replace the port and composer.
pub fn install(cx: &mut App, port: Arc<dyn WorkbookPort>) {
    install_with(cx, port, false);
}

/// Like [`install`], but every leaf shows its workbook read-only and never
/// commits (a host whose note bodies are read-only, such as Ashlar's phone).
pub fn install_read_only(cx: &mut App, port: Arc<dyn WorkbookPort>) {
    install_with(cx, port, true);
}

fn install_with(cx: &mut App, port: Arc<dyn WorkbookPort>, readonly: bool) {
    cx.set_global(BlockPort { port, readonly });
    if !cx.has_global::<LeafCache>() {
        cx.set_global(LeafCache {
            leaves: HashMap::new(),
        });
    }
    register_custom_block(
        cx,
        SPREADSHEET_BLOCK,
        Arc::new(|block, window, cx| Some(compose(block, window, cx))),
    );
}

fn compose(block: &BlockSnapshot, window: &mut Window, cx: &mut App) -> AnyElement {
    let key = block.paint_id().0.clone();
    let reference = block
        .url
        .clone()
        .filter(|url| !url.is_empty())
        .or_else(|| block.props.get("url").cloned())
        .unwrap_or_default();
    let Some((port, readonly)) = cx
        .try_global::<BlockPort>()
        .map(|installed| (installed.port.clone(), installed.readonly))
    else {
        eprintln!("omasheets-block: no workbook port; block {key} paints a placeholder");
        return placeholder("spreadsheet port unavailable");
    };
    if !cx.has_global::<LeafCache>() {
        cx.set_global(LeafCache {
            leaves: HashMap::new(),
        });
    }
    if let Some(existing) = cx.global::<LeafCache>().leaves.get(&key).cloned() {
        existing.update(cx, |leaf, cx| {
            leaf.set_reference(reference, window, cx);
        });
        return existing.into_any_element();
    }
    let leaf = cx.new(|cx| Leaf::new(port, readonly, reference, window, cx));
    cx.global_mut::<LeafCache>()
        .leaves
        .insert(key, leaf.clone());
    leaf.into_any_element()
}

fn placeholder(message: &str) -> AnyElement {
    div()
        .w_full()
        .min_h(px(120.))
        .p_3()
        .border_1()
        .child(SharedString::from(message.to_owned()))
        .into_any_element()
}

struct Leaf {
    port: Arc<dyn WorkbookPort>,
    readonly: bool,
    reference: String,
    state: LeafState,
    /// The version the next commit expects; `None` until the workbook opens.
    version: Option<WorkbookVersion>,
    /// Edits since the workbook opened, and how many of them are saved.
    edits: u64,
    saved: u64,
    /// A save is waiting on its timer or in flight.
    saving: bool,
    /// A save conflicted; nothing more saves while this leaf lives (until the app restarts).
    stopped: bool,
    timer: Option<Task<()>>,
    commit: Option<Task<()>>,
    _open: Option<Task<()>>,
    _ui: Vec<Subscription>,
}

enum LeafState {
    Idle,
    Opening,
    Open(Entity<SpreadsheetView>),
    Failed(String),
}

impl Leaf {
    fn new(
        port: Arc<dyn WorkbookPort>,
        readonly: bool,
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut leaf = Self {
            port,
            readonly,
            reference: String::new(),
            state: LeafState::Idle,
            version: None,
            edits: 0,
            saved: 0,
            saving: false,
            stopped: false,
            timer: None,
            commit: None,
            _open: None,
            _ui: Vec::new(),
        };
        leaf.set_reference(reference, window, cx);
        leaf
    }

    fn set_reference(&mut self, reference: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.reference == reference && !matches!(self.state, LeafState::Idle) {
            return;
        }
        self.reference = reference;
        self.version = None;
        self.reset_saves();
        self._open = None;
        self._ui.clear();
        if self.reference.is_empty() {
            self.state = LeafState::Failed("workbook reference missing".into());
            cx.notify();
            return;
        }
        self.open(window, cx);
    }

    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reference = self.reference.clone();
        let port = self.port.clone();
        self.state = LeafState::Opening;
        cx.notify();
        self._open = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = port.open(&reference).await;
            let _ = this.update_in(cx, |leaf, window, cx| {
                if leaf.reference != reference {
                    return;
                }
                match opened {
                    Ok(read) => {
                        leaf.version = Some(read.version.clone());
                        let label = reference
                            .rsplit('/')
                            .next()
                            .unwrap_or(reference.as_str())
                            .to_string();
                        let content_type = read.content_type.clone();
                        let bytes = read.bytes;
                        leaf.state = LeafState::Opening;
                        leaf._open = Some(cx.spawn_in(window, async move |this, cx| {
                            let session = cx
                                .background_spawn(async move {
                                    SpreadsheetSession::open_bytes(bytes, label, &content_type)
                                })
                                .await;
                            let _ = this.update_in(cx, |leaf, window, cx| {
                                if leaf.reference != reference {
                                    return;
                                }
                                match session {
                                    Ok(session) => leaf.show(session, window, cx),
                                    Err(error) => {
                                        eprintln!(
                                            "omasheets-block: {reference} did not open as a workbook: {error}"
                                        );
                                        leaf.state = LeafState::Failed(error.to_string());
                                        cx.notify();
                                    }
                                }
                            });
                        }));
                    }
                    Err(error) => {
                        eprintln!("omasheets-block: {reference} did not open: {error}");
                        leaf.state = LeafState::Failed(error.to_string());
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn show(&mut self, session: SpreadsheetSession, window: &mut Window, cx: &mut Context<Self>) {
        let view = match &self.state {
            LeafState::Open(existing) => {
                existing.update(cx, |view, cx| {
                    view.show_session(session, window, cx);
                });
                existing.clone()
            }
            // A note embed never takes focus on open; a click or Tab puts it there.
            _ => cx.new(|cx| {
                let mut view = SpreadsheetView::new(session, window, cx);
                view.set_autofocus(false);
                view
            }),
        };
        let readonly = self.readonly;
        view.update(cx, |view, cx| view.set_readonly(readonly, cx));
        self._ui = vec![cx.subscribe(&view, |leaf, _view, event, cx| {
            if matches!(event, SpreadsheetUiEvent::EditCommitted { .. }) {
                leaf.edits += 1;
                leaf.schedule_commit(cx);
            }
        })];
        self.reset_saves();
        self.state = LeafState::Open(view);
        cx.notify();
    }

    fn reset_saves(&mut self) {
        self.edits = 0;
        self.saved = 0;
        self.saving = false;
        self.stopped = false;
        self.timer = None;
        self.commit = None;
    }

    /// Save 1.5 s from now unless a save is already pending; a save that
    /// finishes with newer edits schedules the next one.
    fn schedule_commit(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.stopped || self.readonly || self.edits == self.saved {
            return;
        }
        self.saving = true;
        self.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |leaf, cx| leaf.flush_commit(cx));
        }));
    }

    /// Save the workbook File the block hosts, as the File page control does.
    fn flush_commit(&mut self, cx: &mut Context<Self>) {
        let reference = self.reference.clone();
        let (Some(expected), LeafState::Open(view)) = (self.version.clone(), &self.state) else {
            self.saving = false;
            return;
        };
        let edits = self.edits;
        let bytes = match view.update(cx, |view, _cx| view.session_mut().durable_bytes()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                eprintln!("omasheets-block: {reference} has no native bytes to save");
                self.saving = false;
                return;
            }
            Err(error) => {
                eprintln!("omasheets-block: {reference} did not encode: {error}");
                self.saving = false;
                return;
            }
        };
        let port = self.port.clone();
        let draft = WorkbookDraft {
            expected_version: expected,
            bytes,
        };
        self.commit = Some(cx.spawn(async move |this, cx| {
            let outcome = port.commit(&reference, draft).await;
            let _ = this.update(cx, |leaf, cx| {
                if leaf.reference != reference {
                    return;
                }
                leaf.saving = false;
                match outcome {
                    Ok(version) => {
                        leaf.version = Some(version);
                        leaf.saved = edits;
                        leaf.schedule_commit(cx);
                    }
                    Err(PortError::Conflict { .. }) => {
                        eprintln!(
                            "omasheets-block: {reference} changed elsewhere; edits here are not saved until the app restarts"
                        );
                        leaf.stopped = true;
                    }
                    // The next edit tries again.
                    Err(error) => eprintln!("omasheets-block: {reference} did not save: {error}"),
                }
            });
        }));
    }
}

impl gpui_shell::gpui::Render for Leaf {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl gpui_shell::gpui::IntoElement {
        match &self.state {
            LeafState::Idle | LeafState::Opening => div()
                .w_full()
                .min_h(px(160.))
                .p_3()
                .child("Opening spreadsheet…"),
            LeafState::Failed(message) => div()
                .w_full()
                .min_h(px(120.))
                .p_3()
                .child(SharedString::from(message.clone())),
            LeafState::Open(view) => div().w_full().min_h(px(240.)).child(view.clone()),
        }
    }
}
