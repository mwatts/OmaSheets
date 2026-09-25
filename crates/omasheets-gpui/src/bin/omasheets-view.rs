//! Host window for the gpui spreadsheet view.
//!
//! Picks a workbook from a fixed list under the local Spreadsheet-RL sample
//! and opens it. Arrow keys move the selection, Page Up and Page Down move a
//! screen, and the scroll wheel moves the window.
//!
//! The sample directory is `$OMASHEETS_CORPUS` when that is set, otherwise
//! `~/omasheets-corpus/spreadsheet-rl-2026/sample`.
//!
//! ```text
//! cargo run --release -p omasheets-gpui --bin omasheets-view
//! ```

use std::path::PathBuf;

use gpui_kit::component::Root;
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, IntoElement, MouseButton, ParentElement, Render, SharedString, Styled,
    Subscription, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use omasheets_gpui::{SpreadsheetSession, SpreadsheetView};

fn corpus_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("OMASHEETS_CORPUS") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let mut path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    path.push("omasheets-corpus");
    path.push("spreadsheet-rl-2026");
    path.push("sample");
    path
}

struct CatalogEntry {
    label: &'static str,
    note: &'static str,
    file: &'static str,
}

const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        label: "Debugging 10_01",
        note: "99-sheet operating rollup",
        file: "spreadsheetbench_2__Debugging__10_01__input.xlsx",
    },
    CatalogEntry {
        label: "Financial model 08_04",
        note: "DCF and three statements",
        file: "spreadsheetbench_2__Financial_Model__08_04__input.xlsx",
    },
    CatalogEntry {
        label: "Financial model 04_02",
        note: "Valuation model",
        file: "spreadsheetbench_2__Financial_Model__04_02__input.xlsx",
    },
    CatalogEntry {
        label: "Debugging 01_01",
        note: "LBO model",
        file: "spreadsheetbench_2__Debugging__01_01__input.xlsx",
    },
    CatalogEntry {
        label: "Small styled sheet",
        note: "Verified sample 1_53647",
        file: "spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx",
    },
];

#[derive(Clone)]
struct CorpusFile {
    label: &'static str,
    note: &'static str,
    path: PathBuf,
}

struct CorpusHost {
    spreadsheet: Entity<SpreadsheetView>,
    files: Vec<CorpusFile>,
    selected: Option<usize>,
    busy: bool,
    failed: bool,
    load_token: u64,
    message: String,
    _watch: Subscription,
}

impl CorpusHost {
    fn new(spreadsheet: Entity<SpreadsheetView>, cx: &mut Context<Self>) -> Self {
        let watch = cx.observe(&spreadsheet, |_, _, cx| cx.notify());
        let corpus = corpus_dir();
        let files = CATALOG
            .iter()
            .map(|entry| CorpusFile {
                label: entry.label,
                note: entry.note,
                path: corpus.join(entry.file),
            })
            .collect();
        Self {
            spreadsheet,
            files,
            selected: None,
            busy: false,
            failed: false,
            load_token: 0,
            message: "Choose a workbook.".to_string(),
            _watch: watch,
        }
    }

    fn open_initial(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.files.iter().position(|file| file.path.is_file()) else {
            self.failed = true;
            let root = corpus_dir();
            self.message = format!("No corpus files found under {}", root.display());
            cx.notify();
            return;
        };
        self.choose(index, window, cx);
    }

    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(index) else {
            return;
        };
        self.selected = Some(index);
        if !file.path.is_file() {
            self.failed = true;
            self.busy = false;
            self.message = format!("{} is not on disk ({})", file.label, file.path.display());
            cx.notify();
            return;
        }
        self.load_token = self.load_token.wrapping_add(1);
        let token = self.load_token;
        self.busy = true;
        self.failed = false;
        self.message = format!("Loading {}…", file.label);
        let path = file.path.clone();
        let label = file.label;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { SpreadsheetSession::open_xlsx(&path) })
                .await;
            let _ = this.update_in(cx, |host, window, cx| {
                if host.load_token != token {
                    return;
                }
                host.busy = false;
                match loaded {
                    Ok(session) => {
                        host.failed = false;
                        host.message.clear();
                        host.spreadsheet
                            .update(cx, |view, cx| view.show_session(session, window, cx));
                    }
                    Err(error) => {
                        host.failed = true;
                        host.message = format!("Could not open {label}: {error}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let host = cx.entity();
        let selected = self.selected;
        let rows = self.files.iter().enumerate().map(|(index, file)| {
            let host = host.clone();
            let present = file.path.is_file();
            let label = file.label;
            let note = if present { file.note } else { "not on disk" };
            let background = if selected == Some(index) {
                0xe8f0fe
            } else {
                0xf7f7f7
            };
            div()
                .id(SharedString::from(format!("corpus-file-{index}")))
                .w_full()
                .flex_shrink_0()
                .px_2()
                .py_2()
                .bg(rgb(background))
                .hover(|style| style.bg(rgb(0xeef3fb)))
                .child(label)
                .child(div().text_color(rgb(0x666666)).child(note))
                .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                    host.update(cx, |this, cx| this.choose(index, window, cx));
                })
        });
        div()
            .id("corpus-list")
            .w(px(280.))
            .h_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(rgb(0xf7f7f7))
            .border_r_1()
            .border_color(rgb(0xd0d0d0))
            .child(
                div()
                    .px_2()
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0xd0d0d0))
                    .child("Corpus"),
            )
            .child(
                div()
                    .id("corpus-files")
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .px_2()
                    .py_2()
                    .text_color(rgb(0x555555))
                    .border_t_1()
                    .border_color(rgb(0xd0d0d0))
                    .child("Arrows move the cell. Page Up and Page Down move a screen. Command-arrow jumps. The scroll wheel moves the window."),
            )
    }
}

impl Render for CorpusHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.busy || self.failed {
            self.message.clone()
        } else {
            self.spreadsheet.read(cx).session().summary()
        };
        let status_color = if self.failed { 0x9b1c1c } else { 0x333333 };
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x111111))
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .h(px(36.))
                            .w_full()
                            .px_2()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .bg(rgb(0xf7f7f7))
                            .border_b_1()
                            .border_color(rgb(0xd0d0d0))
                            .text_color(rgb(status_color))
                            .child(status),
                    )
                    .child(self.spreadsheet.clone()),
            )
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.spawn(async move |cx| {
            cx.update(|cx| open_corpus(cx));
        })
        .detach();
    });
}

fn open_corpus(cx: &mut gpui_kit::App) {
    let mut options = WindowOptions::default();
    options.window_bounds = Some(WindowBounds::centered(size(px(1440.), px(900.)), cx));
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(SharedString::from("OmaSheets corpus"));
    }
    cx.open_window(options, |window, cx| {
        let spreadsheet = cx.new(|cx| {
            SpreadsheetView::new(
                SpreadsheetSession::open("OmaSheets").expect("empty session"),
                window,
                cx,
            )
        });
        let host = cx.new(|cx| CorpusHost::new(spreadsheet, cx));
        let host_for_open = host.clone();
        window.defer(cx, move |window, cx| {
            host_for_open.update(cx, |host, cx| host.open_initial(window, cx));
        });
        cx.new(|cx| Root::new(host, window, cx).bordered(false))
    })
    .expect("open the corpus window");
}
