//! The `omasheets.Spreadsheet` control: declaration, factory, and open loop.

use crate::{
    PortError, WorkbookDraft, WorkbookPort, WorkbookRead,
};
use gpui_kit::component::ActiveTheme as _;
use gpui_shell::gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement as _, ParentElement as _,
    Styled as _, Subscription, Task, Window, div, px,
};
use loom_gpui::{
    ControlDeclaration, ControlEvents, ControlMount, ControlRender, JsonType, NativeControl,
    Port, SurfaceControls,
};
use omasheets_gpui::{SpreadsheetSession, SpreadsheetUiEvent, SpreadsheetView};
use serde_json::{Value, json};
use std::{fmt, sync::Arc, time::Duration};

/// The registered type name.
pub const SPREADSHEET: &str = "omasheets.Spreadsheet";

/// Runtime options the view cannot set.
#[derive(Clone, Debug, Default)]
pub struct Options {}

impl fmt::Display for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Options")
    }
}

/// Installs kit bindings the spreadsheet needs. Call once per app.
pub fn init(cx: &mut App) {
    gpui_kit::init(cx);
}

/// The declared API of `omasheets.Spreadsheet`.
pub fn declaration() -> ControlDeclaration {
    let mut declaration = ControlDeclaration::new(
        SPREADSHEET,
        "Spreadsheet grid for one workbook, opened through the consumer's workbook port",
    )
    .bound_prop("workbookRef", JsonType::String)
    .prop("readonly", JsonType::Boolean)
    .prop("focusOnMount", JsonType::Boolean)
    .event(
        "selectionChange",
        "{sheet, a1}: the active cell moved",
    )
    .event(
        "editCommitted",
        "{sheet, a1, source}: the formula bar committed a value",
    )
    .event(
        "commandFailed",
        "{message}: a command or commit was rejected",
    )
    .event(
        "dirtyChange",
        "boolean: whether in-memory edits are not yet durable",
    )
    .event(
        "saveState",
        "`saved`, `failed`, `conflict` or `unavailable`",
    )
    .style_token("density", &["compact", "comfortable", "spacious"])
    .style_token("chrome", &["none", "subtle", "card"])
    .region("toolbarAccessory")
    .requires_port(<dyn WorkbookPort as Port>::NAME);
    for prop in &mut declaration.props {
        match prop.name.as_str() {
            "readonly" | "focusOnMount" => prop.default = Some(json!(false)),
            _ => {}
        }
    }
    declaration.qualifiers = [
        "a spreadsheet",
        "workbook grid",
        "omasheets document",
        "xlsx import",
        "formula sheet",
    ]
    .map(str::to_owned)
    .to_vec();
    declaration
}

/// Registers `omasheets.Spreadsheet` with default options.
pub fn register(controls: &mut SurfaceControls) -> anyhow::Result<()> {
    register_with(controls, Options::default())
}

/// Registers `omasheets.Spreadsheet` with explicit options.
pub fn register_with(controls: &mut SurfaceControls, options: Options) -> anyhow::Result<()> {
    let _ = options;
    controls.register(declaration(), move |mount: ControlMount<'_>, _window, cx| {
        let port = mount
            .ports
            .get::<dyn WorkbookPort>()
            .expect("admission requires the workbook port");
        SpreadsheetControl {
            host: cx.new(|_| Host {
                port,
                events: mount.events.clone(),
                reference: None,
                state: BookState::Idle,
                readonly: false,
                focus_on_mount: false,
                dirty: false,
                version: None,
                commit: None,
                _ui: Vec::new(),
            }),
        }
    })
}

/// The retained side of one `omasheets.Spreadsheet` node.
pub struct SpreadsheetControl {
    host: Entity<Host>,
}

enum BookState {
    Idle,
    Opening(#[allow(dead_code)] Task<()>),
    Open(Entity<SpreadsheetView>),
    Failed(String),
}

struct Host {
    port: Arc<dyn WorkbookPort>,
    events: ControlEvents,
    reference: Option<String>,
    state: BookState,
    readonly: bool,
    focus_on_mount: bool,
    dirty: bool,
    version: Option<String>,
    commit: Option<Task<()>>,
    _ui: Vec<Subscription>,
}

impl Host {
    fn sync(&mut self, props: &Value, _tokens: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let readonly = props["readonly"].as_bool().unwrap_or(false);
        if readonly != self.readonly {
            self.readonly = readonly;
            if let BookState::Open(view) = &self.state {
                view.update(cx, |view, cx| view.set_readonly(readonly, cx));
            }
        }
        let reference = props["workbookRef"].as_str().map(str::to_owned);
        if reference == self.reference {
            return;
        }
        self.discard_local();
        self._ui.clear();
        self.reference = reference.clone();
        let Some(reference) = reference else {
            self.state = BookState::Idle;
            return;
        };
        self.focus_on_mount = props["focusOnMount"].as_bool().unwrap_or(false);
        let open = self.port.open(&reference);
        self.state = BookState::Opening(cx.spawn_in(window, async move |this, cx| {
            let result = open.await;
            let _ = this.update_in(cx, |host, window, cx| {
                if host.reference.as_deref() == Some(reference.as_str()) {
                    host.opened(reference, result, window, cx);
                }
            });
        }));
    }

    fn opened(
        &mut self,
        reference: String,
        result: Result<WorkbookRead, PortError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = match result {
            Ok(read) => read,
            Err(error) => {
                let state = match &error {
                    PortError::Conflict { .. } => "conflict",
                    PortError::Unavailable => "unavailable",
                    PortError::Rejected(_) => "failed",
                };
                self.events.emit("saveState", json!(state));
                self.state = BookState::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        self.version = Some(read.version.clone());
        let label = reference
            .rsplit('/')
            .next()
            .unwrap_or(reference.as_str())
            .to_string();
        let bytes = read.bytes;
        let content_type = read.content_type;
        self.state = BookState::Opening(cx.spawn_in(window, async move |this, cx| {
            let session = cx
                .background_spawn(async move {
                    SpreadsheetSession::open_bytes(bytes, label, &content_type)
                })
                .await;
            let _ = this.update_in(cx, |host, window, cx| {
                if host.reference.as_deref() != Some(reference.as_str()) {
                    return;
                }
                match session {
                    Ok(session) => host.show(session, window, cx),
                    Err(error) => {
                        host.events.emit("saveState", json!("failed"));
                        host.state = BookState::Failed(error.to_string());
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
            BookState::Open(existing) => {
                existing.update(cx, |view, cx| {
                    view.set_readonly(self.readonly, cx);
                    view.show_session(session, window, cx);
                });
                existing.clone()
            }
            _ => {
                let readonly = self.readonly;
                let view = cx.new(|cx| {
                    let mut view = SpreadsheetView::new(session, window, cx);
                    view.set_readonly(readonly, cx);
                    view
                });
                view
            }
        };
        self._ui = vec![cx.subscribe(&view, move |host, _view, event, cx| {
            match event {
                SpreadsheetUiEvent::SelectionChanged { sheet, a1 } => {
                    host.events
                        .emit("selectionChange", json!({ "sheet": sheet, "a1": a1 }));
                }
                SpreadsheetUiEvent::EditCommitted {
                    sheet,
                    a1,
                    source,
                } => {
                    host.on_ui(event, cx);
                    host.events.emit(
                        "editCommitted",
                        json!({ "sheet": sheet, "a1": a1, "source": source }),
                    );
                }
                SpreadsheetUiEvent::CommandFailed { message } => {
                    host.events
                        .emit("commandFailed", json!({ "message": message }));
                }
            }
        })];
        if self.focus_on_mount && !self.readonly {
            // SpreadsheetView focuses its grid on show_session / new.
        }
        self.dirty = false;
        self.events.emit("dirtyChange", json!(false));
        self.events.emit("saveState", json!("saved"));
        self.state = BookState::Open(view);
        cx.notify();
    }

    fn on_ui(&mut self, event: &SpreadsheetUiEvent, cx: &mut Context<Self>) {
        if matches!(event, SpreadsheetUiEvent::EditCommitted { .. }) && !self.dirty {
            self.dirty = true;
            self.events.emit("dirtyChange", json!(true));
            self.schedule_commit(cx);
        }
    }

    fn schedule_commit(&mut self, cx: &mut Context<Self>) {
        self.commit = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |host, cx| {
                host.flush_commit(cx);
            });
        }));
    }

    /// Persist dirty native bytes through the workbook port when possible.
    fn flush_commit(&mut self, cx: &mut Context<Self>) {
        if !self.dirty || self.readonly {
            return;
        }
        let Some(reference) = self.reference.clone() else {
            return;
        };
        let Some(expected) = self.version.clone() else {
            return;
        };
        let BookState::Open(view) = &self.state else {
            return;
        };
        let bytes = match view.update(cx, |view, _cx| view.session_mut().durable_bytes()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.events.emit("saveState", json!("unavailable"));
                return;
            }
            Err(error) => {
                self.events.emit("saveState", json!("failed"));
                self.events
                    .emit("commandFailed", json!({ "message": error.to_string() }));
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
            let _ = this.update(cx, |host, cx| {
                match outcome {
                    Ok(version) => {
                        host.version = Some(version);
                        host.dirty = false;
                        host.events.emit("dirtyChange", json!(false));
                        host.events.emit("saveState", json!("saved"));
                    }
                    Err(PortError::Conflict { current }) => {
                        host.version = Some(current);
                        host.events.emit("saveState", json!("conflict"));
                    }
                    Err(error) => {
                        host.events.emit("saveState", json!("failed"));
                        host.events
                            .emit("commandFailed", json!({ "message": error.to_string() }));
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Drop local dirt when durable commit is impossible (xlsx browse).
    fn discard_local(&mut self) {
        self.commit = None;
        if self.dirty {
            self.dirty = false;
            self.events.emit("dirtyChange", json!(false));
        }
    }
}

impl NativeControl for SpreadsheetControl {
    fn render(
        &mut self,
        input: ControlRender<'_>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.host
            .update(cx, |host, cx| host.sync(input.props, input.tokens, window, cx));
        let tokens = input.tokens;
        let token = |name: &str| tokens[name].as_str().unwrap_or_default();
        let pad = match token("density") {
            "compact" => px(4.),
            "spacious" => px(16.),
            _ => px(8.),
        };
        let host = self.host.read(cx);
        let body: AnyElement = match &host.state {
            BookState::Open(view) => view.clone().into_any_element(),
            BookState::Failed(error) => div()
                .text_color(cx.theme().danger)
                .child(format!("Could not open the workbook: {error}"))
                .into_any_element(),
            BookState::Idle | BookState::Opening(_) => div().into_any_element(),
        };
        let mut regions = input.regions;
        let toolbar = regions
            .iter_mut()
            .find(|(name, _)| name == "toolbarAccessory")
            .map(|(_, kids)| std::mem::take(kids))
            .unwrap_or_default();
        let theme = cx.theme();
        let mut outer = div()
            .flex()
            .flex_col()
            .size_full()
            .gap(pad)
            .children(toolbar)
            .child(div().flex_1().w_full().min_h(px(120.)).child(body));
        outer = match token("chrome") {
            "subtle" => outer.border_1().border_color(theme.border),
            "card" => outer
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius)
                .bg(theme.background)
                .p(pad),
            _ => outer,
        };
        outer.into_any_element()
    }

    fn before_unmount(&mut self, cx: &mut App) {
        self.host.update(cx, |host, cx| {
            if host.dirty {
                host.flush_commit(cx);
            }
            // Native flush is async; if still dirty (xlsx / failed), drop local dirt.
            if host.dirty {
                host.discard_local();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_names_the_workbook_port() {
        let declaration = declaration();
        declaration.check().unwrap();
        assert_eq!(declaration.ty, SPREADSHEET);
        assert!(
            declaration
                .ports
                .iter()
                .any(|port| port.name == "workbook" && port.required)
        );
    }
}
