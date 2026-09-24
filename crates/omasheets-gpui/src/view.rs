//! GPUI view. The document itself is [`crate::SpreadsheetSession`].

use crate::appearance::{AppearanceError, AppearanceTile};
use crate::session::{SpreadsheetSession, VISIBLE_COLUMNS, VISIBLE_ROWS, VisibleCell};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, EventEmitter, IntoElement, MouseButton, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, px, rgb,
};
use omasheets_core::{ApplyError, Command};
use std::path::Path;

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

/// Embeddable spreadsheet. Hosts parent this view and subscribe to [`SpreadsheetUiEvent`].
pub struct SpreadsheetView {
    session: SpreadsheetSession,
    formula: Entity<InputState>,
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
                    this.commit_formula(window, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        )];
        Self {
            session,
            formula,
            _subscriptions: subscriptions,
        }
    }

    pub fn session(&self) -> &SpreadsheetSession {
        &self.session
    }

    /// Applies one document command and notifies. A rejection emits [`SpreadsheetUiEvent::CommandFailed`].
    pub fn apply_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
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

    fn commit_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            }
            Err(error) => self.fail(error, cx),
        }
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
            self.sync_formula(window, cx);
            cx.notify();
        }
    }

    fn sync_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.session.formula_draft().to_string();
        self.formula.update(cx, |input, cx| {
            input.set_value(text, window, cx);
        });
    }

    fn fail(&mut self, error: ApplyError, cx: &mut Context<Self>) {
        cx.emit(SpreadsheetUiEvent::CommandFailed {
            message: error.to_string(),
        });
        cx.notify();
    }

    fn formula_bar(&self) -> impl IntoElement {
        let label = self
            .session
            .selection_a1()
            .unwrap_or_else(|| "—".to_string());
        div()
            .h(px(32.))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .bg(rgb(0xf7f7f7))
            .border_b_1()
            .border_color(rgb(0xd0d0d0))
            .child(div().w(px(72.)).child(label))
            .child(div().flex_1().child(Input::new(&self.formula)))
    }

    fn sheet_labels(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let active = self.session.active_sheet_name().unwrap_or("").to_string();
        let names = self.session.sheet_names();
        div()
            .h(px(28.))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(0xeeeeee))
            .border_t_1()
            .border_color(rgb(0xd0d0d0))
            .children(names.into_iter().enumerate().map(|(index, name)| {
                let selected = name == active;
                let view = view.clone();
                let id = SharedString::from(format!("sheet-tab-{index}"));
                div()
                    .id(id)
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
        let grid = SheetGrid::from_session(&self.session, cx.entity());
        div()
            .id("omasheets-spreadsheet")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x111111))
            .child(self.formula_bar())
            .child(self.sheet_labels(cx))
            .child(grid)
    }
}

/// Custom grid. It builds only the visible window, not a DataTable.
struct SheetGrid {
    view: Entity<SpreadsheetView>,
    column_labels: Vec<String>,
    row_labels: Vec<String>,
    cells: Vec<VisibleCell>,
    column_widths: Vec<f32>,
}

impl SheetGrid {
    fn from_session(session: &SpreadsheetSession, view: Entity<SpreadsheetView>) -> Self {
        let origin_column = session.visible_window().origin_column as usize;
        let column_widths = (0..VISIBLE_COLUMNS)
            .map(|offset| session.column_width_px(origin_column + offset as usize))
            .collect();
        Self {
            view,
            column_labels: session.column_labels(),
            row_labels: session.row_labels(),
            cells: session.visible_cells(),
            column_widths,
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
                    .w(px(48.))
                    .h(px(22.))
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0)),
            ];
            headers.extend(self.column_labels.iter().enumerate().map(|(index, label)| {
                let width = self.column_widths.get(index).copied().unwrap_or(72.0);
                div()
                    .w(px(width))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0))
                    .child(label.clone())
            }));
            headers
        });

        let mut rows = Vec::with_capacity(VISIBLE_ROWS as usize);
        for row_offset in 0..VISIBLE_ROWS as usize {
            let label = self.row_labels.get(row_offset).cloned().unwrap_or_default();
            let mut row = div().flex().flex_row().child(
                div()
                    .w(px(48.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0xf3f3f3))
                    .border_1()
                    .border_color(rgb(0xd0d0d0))
                    .child(label),
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
                row = row.child(
                    div()
                        .id(id)
                        .w(px(width))
                        .h(px(22.))
                        .px_1()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .bg(rgb(fill))
                        .text_color(rgb(font))
                        .border_1()
                        .border_color(rgb(border))
                        .child(cell.text.clone())
                        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                            view.update(cx, |this, cx| {
                                this.select_cell(row_index, column_index, window, cx);
                            });
                        }),
                );
            }
            rows.push(row);
        }

        div()
            .id("omasheets-grid")
            .flex_1()
            .overflow_hidden()
            .child(header)
            .children(rows)
    }
}
