//! GPUI view. The document itself is [`crate::SpreadsheetSession`].

use crate::appearance::{AppearanceError, AppearanceTile};
use crate::session::{SpreadsheetSession, VISIBLE_COLUMNS, VISIBLE_ROWS, VisibleCell};
use gpui_kit::component::input::{Enter, Escape, Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseMoveEvent, ParentElement, Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled,
    Subscription, TestSupportExt, Window, div, px, rgb,
};
use omasheets_core::{ApplyError, Command};
use std::path::Path;

const FORMULA_BAR_PX: f32 = 32.0;
const SHEET_TABS_PX: f32 = 28.0;
const HEADER_PX: f32 = 22.0;
const ROW_HEADER_PX: f32 = 48.0;

/// Events a host receives through `cx.subscribe`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpreadsheetUiEvent {
    SelectionChanged {
        sheet: String,
        a1: String,
    },
    EditCommitted {
        sheet: String,
        a1: String,
        source: String,
    },
    CommandFailed {
        message: String,
    },
}

/// A header-edge drag. The view keeps it so a redraw does not drop the gesture.
#[derive(Clone, Copy)]
struct SizeDrag {
    axis: ResizeAxis,
    origin: f32,
    start: f32,
}

#[derive(Clone, Copy)]
enum ResizeAxis {
    Column(usize),
    Row(usize),
}

/// Embeddable spreadsheet. Hosts parent this view and subscribe to [`SpreadsheetUiEvent`].
pub struct SpreadsheetView {
    session: SpreadsheetSession,
    formula: Entity<InputState>,
    grid_focus: FocusHandle,
    scroll_rows: f32,
    scroll_cols: f32,
    chrome_epoch: u64,
    resize: Option<SizeDrag>,
    readonly: bool,
    /// Focus the grid when a document is shown (off for a leaf embedded in a note).
    autofocus: bool,
    /// Phone gestures: a tap selects, a double-tap or a tap on the formula field edits.
    touch: bool,
    /// The formula field holds the keyboard for the selected cell.
    editing: bool,
    _subscriptions: Vec<Subscription>,
}

impl SpreadsheetView {
    pub fn new(session: SpreadsheetSession, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial = session.formula_draft().to_string();
        let formula = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("formula")
                .default_value(initial)
        });
        let subscriptions = vec![cx.subscribe_in(
            &formula,
            window,
            |this, input, event, window, cx| match event {
                InputEvent::Change => {
                    this.session
                        .set_formula_draft(input.read(cx).value().to_string());
                }
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.session.set_formula_draft(text);
                    if this.commit_formula(window, cx) {
                        // As in any spreadsheet: the edit lands and the cell below is next.
                        this.navigate(1, 0, window, cx);
                        if this.touch {
                            // The keyboard stays up so a column is entered in one go.
                            this.begin_edit(None, window, cx);
                        } else {
                            this.grid_focus.focus(window, cx);
                        }
                    }
                }
                InputEvent::Focus => {
                    if this.readonly {
                        this.grid_focus.focus(window, cx);
                    } else {
                        this.editing = true;
                        cx.notify();
                    }
                }
                InputEvent::Blur => {
                    // On touch, Done or a tap outside the field commits the edit.
                    if this.touch && this.editing {
                        this.session
                            .set_formula_draft(input.read(cx).value().to_string());
                        this.commit_formula(window, cx);
                    }
                    if this.editing {
                        this.editing = false;
                        cx.notify();
                    }
                }
            },
        )];
        let view = Self {
            session,
            formula,
            grid_focus: cx.focus_handle(),
            scroll_rows: 0.0,
            scroll_cols: 0.0,
            chrome_epoch: 0,
            resize: None,
            readonly: false,
            autofocus: true,
            touch: false,
            editing: false,
            _subscriptions: subscriptions,
        };
        cx.defer_in(window, |this, window, cx| {
            if this.autofocus {
                this.grid_focus.focus(window, cx);
            }
        });
        view
    }

    pub fn session(&self) -> &SpreadsheetSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SpreadsheetSession {
        &mut self.session
    }

    /// When true, formula commits and host commands are rejected.
    pub fn set_readonly(&mut self, readonly: bool, cx: &mut Context<Self>) {
        if self.readonly != readonly {
            self.readonly = readonly;
            cx.notify();
        }
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    /// Phone gestures (off by default). A tap on a cell selects it without
    /// raising the keyboard; a double-tap, or a tap on the formula field,
    /// edits it; while editing the formula field shows commit and discard.
    pub fn set_touch(&mut self, touch: bool, cx: &mut Context<Self>) {
        if self.touch != touch {
            self.touch = touch;
            cx.notify();
        }
    }

    pub fn touch(&self) -> bool {
        self.touch
    }

    /// Whether the formula field is editing the selected cell.
    pub fn editing(&self) -> bool {
        self.editing
    }

    /// Height for an embed: the used rows plus one empty row, 5 to 15 rows,
    /// below the formula bar, sheet tabs, and column header.
    pub fn embed_height_px(&self) -> f32 {
        let rows = self.session.embed_rows();
        let body: f32 = (0..rows).map(|row| self.session.row_height_px(row)).sum();
        FORMULA_BAR_PX + SHEET_TABS_PX + HEADER_PX + body
    }

    /// Whether showing a document takes keyboard focus. A host that embeds the
    /// view among other controls turns this off so opening never steals focus.
    pub fn set_autofocus(&mut self, autofocus: bool) {
        self.autofocus = autofocus;
    }

    /// Replaces the document the grid is showing and, with autofocus on, focuses the grid.
    pub fn show_session(
        &mut self,
        session: SpreadsheetSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session = session;
        self.resize = None;
        self.scroll_rows = 0.0;
        self.scroll_cols = 0.0;
        self.sync_formula(window, cx);
        self.request_chrome(window, cx);
        cx.defer_in(window, |this, window, cx| {
            if this.autofocus {
                this.grid_focus.focus(window, cx);
            }
        });
        cx.notify();
    }

    /// Applies one document command and notifies. A rejection emits [`SpreadsheetUiEvent::CommandFailed`].
    pub fn apply_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        if self.readonly {
            self.fail_message("workbook is read-only", cx);
            return;
        }
        match self.session.apply_command(command) {
            Ok(()) => {
                self.sync_formula(window, cx);
                cx.notify();
            }
            Err(error) => self.fail(error, cx),
        }
    }

    /// Projects package styles onto the visible tile. Values stay on the document.
    pub fn load_appearance(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Result<(), AppearanceError> {
        let tile = crate::project_xlsx_appearance(path, self.session.visible_window())?;
        self.show_appearance(tile, cx);
        Ok(())
    }

    pub fn show_appearance(&mut self, tile: AppearanceTile, cx: &mut Context<Self>) {
        self.session.set_appearance(tile);
        cx.notify();
    }

    /// Commits the draft into the selected cell; false when it was refused.
    fn commit_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.readonly {
            self.fail_message("workbook is read-only", cx);
            return false;
        }
        match self.session.commit_edit() {
            Ok((address, source)) => {
                let sheet = self
                    .session
                    .active_sheet_name()
                    .unwrap_or("Sheet")
                    .to_string();
                let a1 = format!(
                    "{}{}",
                    omasheets_core::column_letters(address.column),
                    address.row + 1
                );
                self.sync_formula(window, cx);
                cx.emit(SpreadsheetUiEvent::EditCommitted { sheet, a1, source });
                cx.notify();
                true
            }
            Err(error) => {
                self.fail(error, cx);
                false
            }
        }
    }

    /// Starts editing the selected cell in the formula field: with `typed`
    /// replacing its contents, or with its current contents. The caret ends
    /// the text so typing continues it.
    fn begin_edit(&mut self, typed: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.readonly {
            return;
        }
        self.editing = true;
        if let Some(text) = typed {
            self.session.set_formula_draft(text);
        }
        self.sync_formula(window, cx);
        self.formula.update(cx, |input, cx| {
            input.focus(window, cx);
            let end = input.value().len();
            input.set_selected_range(end..end, cx);
        });
        cx.notify();
    }

    /// Escape in the formula field: drop the draft and give the grid the keyboard.
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = false;
        if let Some(address) = self.session.selection() {
            let _ = self.session.select(address.row, address.column);
        }
        self.sync_formula(window, cx);
        self.grid_focus.focus(window, cx);
        cx.notify();
    }

    /// The ✓ beside the formula field: commit and leave the cell selected.
    fn commit_and_end_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.formula.read(cx).value().to_string();
        self.session.set_formula_draft(text);
        self.editing = false;
        self.commit_formula(window, cx);
        self.grid_focus.focus(window, cx);
        cx.notify();
    }

    /// A press on a cell. On touch a single tap only selects; a double-tap edits.
    fn press_cell(
        &mut self,
        row: usize,
        column: usize,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize = None;
        if !self.touch {
            self.grid_focus.focus(window, cx);
            self.select_cell(row, column, window, cx);
            return;
        }
        let same = self.session.selection() == Some(crate::session::CellAddress { row, column });
        if self.editing && !(same && click_count >= 2) {
            // Leaving the field by tapping a cell commits, as a blur does.
            self.commit_and_end_edit(window, cx);
        }
        if !same {
            self.select_cell(row, column, window, cx);
        }
        if click_count >= 2 && !self.readonly {
            self.begin_edit(None, window, cx);
        } else if !self.editing {
            self.grid_focus.focus(window, cx);
        }
    }

    fn drag_resize(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            self.resize = None;
            return;
        }
        let Some(drag) = self.resize else {
            return;
        };
        let current = match drag.axis {
            ResizeAxis::Column(_) => event.position.x.as_f32(),
            ResizeAxis::Row(_) => event.position.y.as_f32(),
        };
        let next = drag.start + (current - drag.origin);
        match drag.axis {
            ResizeAxis::Column(column) => self.session.set_column_width_px(column, next),
            ResizeAxis::Row(row) => self.session.set_row_height_px(row, next),
        }
        cx.notify();
    }

    fn select_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.session.select(row, column) {
            Ok(address) => {
                let sheet = self
                    .session
                    .active_sheet_name()
                    .unwrap_or("Sheet")
                    .to_string();
                let a1 = format!(
                    "{}{}",
                    omasheets_core::column_letters(address.column),
                    address.row + 1
                );
                self.sync_formula(window, cx);
                cx.emit(SpreadsheetUiEvent::SelectionChanged { sheet, a1 });
                cx.notify();
            }
            Err(error) => self.fail(error, cx),
        }
    }

    fn activate_sheet(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.activate_sheet(index) {
            self.scroll_rows = 0.0;
            self.scroll_cols = 0.0;
            self.sync_formula(window, cx);
            self.request_chrome(window, cx);
            self.grid_focus.focus(window, cx);
            cx.notify();
        }
    }

    fn navigate(
        &mut self,
        row_delta: i32,
        column_delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.session.move_selection(row_delta, column_delta) {
            Ok(address) => {
                self.session.retile();
                let sheet = self
                    .session
                    .active_sheet_name()
                    .unwrap_or("Sheet")
                    .to_string();
                let a1 = format!(
                    "{}{}",
                    omasheets_core::column_letters(address.column),
                    address.row + 1
                );
                self.sync_formula(window, cx);
                cx.emit(SpreadsheetUiEvent::SelectionChanged { sheet, a1 });
                cx.notify();
            }
            Err(error) => self.fail(error, cx),
        }
    }

    fn on_grid_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.grid_focus.is_focused(window) || event.keystroke.modifiers.alt {
            return false;
        }
        let jump = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
        if !self.readonly && !jump && self.session.selection().is_some() {
            let key = event.keystroke.key.as_str();
            if (key == "enter" || key == "f2") && !event.keystroke.modifiers.shift {
                self.begin_edit(None, window, cx);
                return true;
            }
            let typed = event
                .keystroke
                .key_char
                .as_ref()
                .filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
            if let Some(text) = typed {
                self.begin_edit(Some(text.clone()), window, cx);
                return true;
            }
        }
        let far_back = i32::MIN;
        let far_forward = i32::MAX;
        let (rows, columns) = match event.keystroke.key.as_str() {
            "up" => (if jump { far_back } else { -1 }, 0),
            "down" => (if jump { far_forward } else { 1 }, 0),
            "left" => (0, if jump { far_back } else { -1 }),
            "right" => (0, if jump { far_forward } else { 1 }),
            "pageup" => (-(VISIBLE_ROWS as i32), 0),
            "pagedown" => (VISIBLE_ROWS as i32, 0),
            "home" => {
                if jump {
                    (far_back, far_back)
                } else {
                    (0, far_back)
                }
            }
            "end" => {
                if jump {
                    (far_forward, far_forward)
                } else {
                    (0, far_forward)
                }
            }
            _ => return false,
        };
        self.navigate(rows, columns, window, cx);
        true
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let (mut rows, mut columns) = match event.delta {
            ScrollDelta::Lines(point) => (-point.y, -point.x),
            ScrollDelta::Pixels(point) => (-(point.y / px(22.0)), -(point.x / px(72.0))),
        };
        if event.modifiers.shift {
            std::mem::swap(&mut rows, &mut columns);
        }
        self.scroll_rows += rows;
        self.scroll_cols += columns;
        let row_steps = self.scroll_rows.trunc() as i32;
        let column_steps = self.scroll_cols.trunc() as i32;
        self.scroll_rows -= row_steps as f32;
        self.scroll_cols -= column_steps as f32;
        if row_steps == 0 && column_steps == 0 {
            return;
        }
        self.session.scroll_by(row_steps, column_steps);
        self.session.retile();
        cx.notify();
    }

    fn request_chrome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.session.workbook_path().map(|path| path.to_path_buf()) else {
            return;
        };
        let Some(name) = self.session.chrome_sheet_name().map(str::to_string) else {
            return;
        };
        let index = self.session.active_index();
        self.chrome_epoch = self.chrome_epoch.wrapping_add(1);
        let epoch = self.chrome_epoch;
        cx.spawn_in(window, async move |this, cx| {
            let chrome = cx
                .background_spawn(
                    async move { crate::appearance::load_sheet_chrome(&path, &name).ok() },
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.chrome_epoch != epoch {
                    return;
                }
                view.session.install_chrome(index, chrome);
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.session.formula_draft().to_string();
        self.formula.update(cx, |input, cx| {
            input.set_value(text, window, cx);
        });
    }

    fn fail(&mut self, error: ApplyError, cx: &mut Context<Self>) {
        self.fail_message(error.to_string(), cx);
    }

    fn fail_message(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(SpreadsheetUiEvent::CommandFailed {
            message: message.into(),
        });
        cx.notify();
    }

    fn formula_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .session
            .selection_a1()
            .unwrap_or_else(|| "—".to_string());
        let view = cx.entity();
        let edit_buttons = (self.touch && self.editing && !self.readonly).then(|| {
            let commit = view.clone();
            let discard = view.clone();
            div()
                .flex_none()
                .flex()
                .flex_row()
                .gap_1()
                .child(
                    div()
                        .id("omasheets-formula-commit")
                        .aria_label("Commit")
                        .test_support()
                        .w(px(28.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child("✓")
                        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                            cx.stop_propagation();
                            commit.update(cx, |this, cx| this.commit_and_end_edit(window, cx));
                        }),
                )
                .child(
                    div()
                        .id("omasheets-formula-discard")
                        .aria_label("Discard")
                        .test_support()
                        .w(px(28.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child("✕")
                        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                            cx.stop_propagation();
                            discard.update(cx, |this, cx| this.cancel_edit(window, cx));
                        }),
                )
        });
        div()
            .h(px(FORMULA_BAR_PX))
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .bg(rgb(0xf7f7f7))
            .border_b_1()
            .border_color(rgb(0xd0d0d0))
            .child(div().flex_none().w(px(72.)).child(label))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    // Escape discards the edit here rather than leaving a host's embed.
                    .on_action(cx.listener(|this, _: &Escape, window, cx| {
                        this.cancel_edit(window, cx);
                    }))
                    // The single-line input commits on Enter and lets it
                    // propagate; the edit is done, so it stops here.
                    .on_action(|_: &Enter, _, cx| cx.stop_propagation())
                    // A read-only embed shows the cell but never takes the keyboard.
                    .child(Input::new(&self.formula).disabled(self.readonly)),
            )
            .children(edit_buttons)
    }

    fn sheet_labels(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let active = self.session.active_sheet_name().unwrap_or("").to_string();
        let names = self.session.sheet_names();
        div()
            .id("omasheets-sheet-tabs")
            .h(px(SHEET_TABS_PX))
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .flex_nowrap()
            .items_center()
            .overflow_x_scroll()
            .bg(rgb(0xeeeeee))
            .border_t_1()
            .border_color(rgb(0xd0d0d0))
            .children(names.into_iter().enumerate().map(|(index, name)| {
                let selected = name == active;
                let view = view.clone();
                let id = SharedString::from(format!("sheet-tab-{index}"));
                div()
                    .id(id)
                    .flex_shrink_0()
                    .h_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(rgb(if selected { 0xffffff } else { 0xe4e4e4 }))
                    .child(name)
                    .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                        view.update(cx, |this, cx| this.activate_sheet(index, window, cx));
                    })
            }))
    }
}

impl EventEmitter<SpreadsheetUiEvent> for SpreadsheetView {}

impl Render for SpreadsheetView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grid = SheetGrid::from_session(&self.session, cx.entity(), self.grid_focus.clone());
        div()
            .id("omasheets-spreadsheet")
            .debug_selector(|| "omasheets-embed".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x111111))
            .child(self.formula_bar(cx))
            .child(self.sheet_labels(cx))
            .child(grid)
    }
}

/// Custom grid. It builds only the visible window, not a DataTable.
struct SheetGrid {
    view: Entity<SpreadsheetView>,
    focus: FocusHandle,
    column_labels: Vec<String>,
    row_labels: Vec<String>,
    cells: Vec<VisibleCell>,
    column_widths: Vec<f32>,
    row_heights: Vec<f32>,
}

impl SheetGrid {
    fn from_session(
        session: &SpreadsheetSession,
        view: Entity<SpreadsheetView>,
        focus: FocusHandle,
    ) -> Self {
        let origin = session.visible_window();
        let origin_column = origin.origin_column as usize;
        let origin_row = origin.origin_row as usize;
        let column_widths = (0..VISIBLE_COLUMNS)
            .map(|offset| session.column_width_px(origin_column + offset as usize))
            .collect();
        let row_heights = (0..VISIBLE_ROWS)
            .map(|offset| session.row_height_px(origin_row + offset as usize))
            .collect();
        Self {
            view,
            focus,
            column_labels: session.column_labels(),
            row_labels: session.row_labels(),
            cells: session.visible_cells(),
            column_widths,
            row_heights,
        }
    }
}

impl IntoElement for SheetGrid {
    type Element = gpui_kit::ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        gpui_kit::ViewElement::new(self)
    }
}

impl RenderOnce for SheetGrid {
    fn render(self, _window: &mut Window, _cx: &mut gpui_kit::App) -> impl IntoElement {
        let header = div().flex().flex_row().children({
            let mut headers = vec![
                div()
                    .id("omasheets-corner")
                    .test_support()
                    .flex_none()
                    .w(px(ROW_HEADER_PX))
                    .h(px(HEADER_PX))
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0)),
            ];
            headers.extend(self.column_labels.iter().enumerate().map(|(index, label)| {
                let width = self.column_widths.get(index).copied().unwrap_or(72.0);
                let column = self
                    .cells
                    .get(index)
                    .map(|cell| cell.column)
                    .unwrap_or(index);
                div()
                    .id(SharedString::from(format!("column-header-{column}")))
                    .test_support()
                    .flex_none()
                    .w(px(width))
                    .h(px(HEADER_PX))
                    .flex()
                    .flex_row()
                    .items_center()
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .child(label.clone()),
                    )
                    .child(edge_handle(
                        self.view.clone(),
                        ResizeAxis::Column(column),
                        6.0,
                        22.0,
                    ))
            }));
            headers
        });

        let mut rows = Vec::with_capacity(VISIBLE_ROWS as usize);
        for row_offset in 0..VISIBLE_ROWS as usize {
            let label = self.row_labels.get(row_offset).cloned().unwrap_or_default();
            let height = self.row_heights.get(row_offset).copied().unwrap_or(22.0);
            let row_index = self
                .cells
                .get(row_offset * VISIBLE_COLUMNS as usize)
                .map(|cell| cell.row)
                .unwrap_or(row_offset);
            let mut row = div().flex().flex_row().child(
                div()
                    .flex_none()
                    .w(px(ROW_HEADER_PX))
                    .h(px(height))
                    .flex()
                    .flex_col()
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .child(label),
                    )
                    .child(edge_handle(
                        self.view.clone(),
                        ResizeAxis::Row(row_index),
                        48.0,
                        6.0,
                    )),
            );
            for column_offset in 0..VISIBLE_COLUMNS as usize {
                let index = row_offset * VISIBLE_COLUMNS as usize + column_offset;
                let Some(cell) = self.cells.get(index) else {
                    continue;
                };
                let width = self
                    .column_widths
                    .get(column_offset)
                    .copied()
                    .unwrap_or(72.0);
                let fill = cell
                    .fill
                    .unwrap_or(if cell.selected { 0xe8f0fe } else { 0xffffff });
                let font = cell.font.unwrap_or(0x111111);
                let border = if cell.selected {
                    0x1a73e8
                } else if cell.merged {
                    0xb0b0b0
                } else {
                    0xe2e2e2
                };
                let view = self.view.clone();
                let row_index = cell.row;
                let column_index = cell.column;
                let id = SharedString::from(format!("cell-{row_index}-{column_index}"));
                let align = if cell.numeric {
                    div().justify_end()
                } else {
                    div().justify_start()
                };
                row = row.child(
                    align
                        .id(id)
                        .aria_label(cell.text.clone())
                        .test_support()
                        .flex_none()
                        .w(px(width))
                        .h(px(height))
                        .px_1()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .bg(rgb(fill))
                        .text_color(rgb(font))
                        .border_1()
                        .border_color(rgb(border))
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "cell-text-{row_index}-{column_index}"
                                )))
                                .test_support()
                                .child(cell.text.clone()),
                        )
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            // The cell decides focus; the grid's own press would
                            // take it back from an edit a double-tap just began.
                            cx.stop_propagation();
                            view.update(cx, |this, cx| {
                                this.press_cell(
                                    row_index,
                                    column_index,
                                    event.click_count,
                                    window,
                                    cx,
                                );
                            });
                        }),
                );
            }
            rows.push(row);
        }

        let view = self.view.clone();
        let focus = self.focus.clone();
        div()
            .id("omasheets-grid")
            .debug_selector(|| "omasheets-grid".into())
            .flex_1()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let handled = view.update(cx, |this, cx| this.on_grid_key(event, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            })
            .on_scroll_wheel({
                let view = self.view.clone();
                move |event: &ScrollWheelEvent, _window, cx| {
                    view.update(cx, |this, cx| this.on_wheel(event, cx));
                    cx.stop_propagation();
                }
            })
            .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                focus.focus(window, cx);
            })
            .on_mouse_move({
                let view = self.view.clone();
                move |event: &MouseMoveEvent, _window, cx| {
                    view.update(cx, |this, cx| this.drag_resize(event, cx));
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let view = self.view.clone();
                move |_event, _window, cx| {
                    view.update(cx, |this, cx| {
                        if this.resize.take().is_some() {
                            cx.notify();
                        }
                    });
                }
            })
            .child(header)
            .children(rows)
    }
}

/// The header border is the hit target. GPUI's mouse-down callback does not
/// include the element bounds, so the edge is its own element.
fn edge_handle(
    view: Entity<SpreadsheetView>,
    axis: ResizeAxis,
    width: f32,
    height: f32,
) -> gpui_kit::Div {
    div()
        .w(px(width))
        .h(px(height))
        .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
            cx.stop_propagation();
            let position = match axis {
                ResizeAxis::Column(_) => event.position.x.as_f32(),
                ResizeAxis::Row(_) => event.position.y.as_f32(),
            };
            view.update(cx, |this, cx| {
                if event.click_count >= 2 {
                    this.resize = None;
                    match axis {
                        ResizeAxis::Column(column) => this.session.autofit_column(column),
                        ResizeAxis::Row(row) => this.session.autofit_row(row),
                    }
                } else {
                    let start = match axis {
                        ResizeAxis::Column(column) => this.session.column_width_px(column),
                        ResizeAxis::Row(row) => this.session.row_height_px(row),
                    };
                    this.resize = Some(SizeDrag {
                        axis,
                        origin: position,
                        start,
                    });
                }
                cx.notify();
            });
        })
}
