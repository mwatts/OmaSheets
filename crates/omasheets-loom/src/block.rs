//! Custom `spreadsheet` block leaf composed into the block editor from outside.
//!
//! The kit paints `BlockType::Custom` as a label unless a host registers a
//! composer. Ashlar (and other Loom hosts) call [`install`] with a
//! [`WorkbookPort`] so note compositions can embed a workbook by reference
//! without forking block-view.

use crate::WorkbookPort;
use gpui_component_block_view::{BlockSnapshot, register_custom_block};
use gpui_shell::gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Global, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use omasheets_gpui::{SpreadsheetSession, SpreadsheetView};
use std::collections::HashMap;
use std::sync::Arc;

/// `BlockType::Custom` name for an embedded workbook leaf.
pub const SPREADSHEET_BLOCK: &str = "spreadsheet";

struct BlockPort(Arc<dyn WorkbookPort>);

impl Global for BlockPort {}

struct LeafCache {
    leaves: HashMap<String, Entity<Leaf>>,
}

impl Global for LeafCache {}

/// Register the spreadsheet custom-block composer for this app.
///
/// Safe to call more than once; later calls replace the port and composer.
pub fn install(cx: &mut App, port: Arc<dyn WorkbookPort>) {
    cx.set_global(BlockPort(port));
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
    let Some(port) = cx.try_global::<BlockPort>().map(|installed| installed.0.clone()) else {
        return placeholder("spreadsheet port unavailable");
    };
    if !cx.has_global::<LeafCache>() {
        cx.set_global(LeafCache {
            leaves: HashMap::new(),
        });
    }
    if let Some(existing) = cx
        .global::<LeafCache>()
        .leaves
        .get(&key)
        .cloned()
    {
        existing.update(cx, |leaf, cx| {
            leaf.set_reference(reference, window, cx);
        });
        return existing.into_any_element();
    }
    let leaf = cx.new(|cx| Leaf::new(port, reference, window, cx));
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
    reference: String,
    state: LeafState,
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
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut leaf = Self {
            port,
            reference: String::new(),
            state: LeafState::Idle,
            _open: None,
            _ui: Vec::new(),
        };
        leaf.set_reference(reference, window, cx);
        leaf
    }

    fn set_reference(
        &mut self,
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reference == reference && !matches!(self.state, LeafState::Idle) {
            return;
        }
        self.reference = reference;
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
                        let label = reference
                            .rsplit('/')
                            .next()
                            .unwrap_or(reference.as_str())
                            .to_string();
                        let content_type = read.content_type.clone();
                        let bytes = read.bytes;
                        leaf.state = LeafState::Opening;
                        leaf._open =
                            Some(cx.spawn_in(window, async move |this, cx| {
                                let session = cx
                                    .background_spawn(async move {
                                        SpreadsheetSession::open_bytes(
                                            bytes,
                                            label,
                                            &content_type,
                                        )
                                    })
                                    .await;
                                let _ = this.update_in(cx, |leaf, window, cx| {
                                    if leaf.reference != reference {
                                        return;
                                    }
                                    match session {
                                        Ok(session) => leaf.show(session, window, cx),
                                        Err(error) => {
                                            leaf.state = LeafState::Failed(error.to_string());
                                            cx.notify();
                                        }
                                    }
                                });
                            }));
                    }
                    Err(error) => {
                        leaf.state = LeafState::Failed(error.to_string());
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn show(
        &mut self,
        session: SpreadsheetSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = match &self.state {
            LeafState::Open(existing) => {
                existing.update(cx, |view, cx| {
                    view.show_session(session, window, cx);
                });
                existing.clone()
            }
            _ => cx.new(|cx| SpreadsheetView::new(session, window, cx)),
        };
        self.state = LeafState::Open(view);
        cx.notify();
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
            LeafState::Open(view) => div()
                .w_full()
                .min_h(px(240.))
                .child(view.clone()),
        }
    }
}
