//! Headless checks of the production [`SpreadsheetView`].
//! The view is not reimplemented here.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Subscription, TestAppContext, Window, WindowHandle, div, px, size,
};
use omasheets_core::Command;
use omasheets_gpui::{
    RgbColor, SpreadsheetSession, SpreadsheetUiEvent, SpreadsheetView, VisibleWindow,
    project_xlsx_appearance,
};

const SAMPLE_53647: &str = "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx";
const FILL_RGB: &str = "FFC41E3A";
const COLUMN_WIDTH: f64 = 20.0;
const CELL_HEIGHT_PX: f32 = 22.0;

struct Host {
    spreadsheet: Entity<SpreadsheetView>,
    _subscription: Subscription,
}

impl Host {
    fn new(
        spreadsheet: Entity<SpreadsheetView>,
        events: Rc<RefCell<Vec<SpreadsheetUiEvent>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&spreadsheet, move |_host, _view, event, _cx| {
            events.borrow_mut().push(event.clone());
        });
        Self {
            spreadsheet,
            _subscription: subscription,
        }
    }
}

impl Render for Host {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.spreadsheet.clone())
    }
}

struct Opened {
    window: WindowHandle<Root>,
    spreadsheet: Entity<SpreadsheetView>,
    events: Rc<RefCell<Vec<SpreadsheetUiEvent>>>,
    /// Left presses that bubbled out of the spreadsheet to its host.
    host_presses: Rc<std::cell::Cell<usize>>,
}

/// A note's embed: as wide as the window and as tall as the view asks,
/// with a host press handler like the block editor's.
struct EmbedHost {
    spreadsheet: Entity<SpreadsheetView>,
    presses: Rc<std::cell::Cell<usize>>,
    _subscriptions: Vec<Subscription>,
}

impl Render for EmbedHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let presses = self.presses.clone();
        let height = self.spreadsheet.read(cx).embed_height_px();
        div()
            .size_full()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, _| {
                presses.set(presses.get() + 1);
            })
            .child(
                div()
                    .w_full()
                    .h(px(height))
                    .overflow_hidden()
                    .child(self.spreadsheet.clone()),
            )
    }
}

fn open_spreadsheet(cx: &mut TestAppContext) -> Opened {
    cx.update(gpui_kit::init);
    let events = Rc::new(RefCell::new(Vec::new()));
    let sink = events.clone();
    let slot: Rc<RefCell<Option<Entity<SpreadsheetView>>>> = Rc::new(RefCell::new(None));
    let slot_for_open = slot.clone();
    let window = cx.open_window(size(px(1600.), px(1100.)), move |window, cx| {
        let spreadsheet = cx.new(|cx| {
            SpreadsheetView::new(
                SpreadsheetSession::open("book").expect("session opens"),
                window,
                cx,
            )
        });
        slot_for_open.borrow_mut().replace(spreadsheet.clone());
        let host = cx.new(|cx| Host::new(spreadsheet, sink, cx));
        Root::new(host, window, cx)
    });
    Opened {
        window,
        spreadsheet: slot.borrow().clone().expect("spreadsheet mounted"),
        events,
        host_presses: Rc::default(),
    }
}

fn formula_bar_id(window: &Window) -> gpui_kit::ElementId {
    gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .find(|snapshot| snapshot.label() == Some("formula"))
        .and_then(|snapshot| snapshot.path().last().cloned())
        .unwrap_or_else(|| {
            panic!(
                "formula bar should be findable by its placeholder. Registered paths: {}",
                gpui_kit::base::test_support::registered_paths(window)
            )
        })
}

fn interact(
    opened: &Opened,
    cx: &mut TestAppContext,
    action: impl FnOnce(&mut Window, &mut gpui_kit::App),
) {
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        action(window, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn type_formula(opened: &Opened, cx: &mut TestAppContext, source: &str) {
    interact(opened, cx, |window, cx| {
        window.click(formula_bar_id(window), cx)
    });
    interact(opened, cx, |window, cx| window.input(source, cx));
    interact(opened, cx, |window, cx| window.press("enter", cx));
}

fn events(opened: &Opened) -> Vec<SpreadsheetUiEvent> {
    opened.events.borrow().clone()
}

fn clear_events(opened: &Opened) {
    opened.events.borrow_mut().clear();
}

#[gpui_kit::test]
fn clicking_a_cell_and_entering_a_formula_commits_and_displays_the_result(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    interact(&opened, cx, |window, cx| window.click("cell-0-0", cx));
    type_formula(&opened, cx, "=1+2");
    let committed = events(&opened)
        .into_iter()
        .find(|event| matches!(event, SpreadsheetUiEvent::EditCommitted { .. }));
    assert_eq!(
        committed,
        Some(SpreadsheetUiEvent::EditCommitted {
            sheet: "Sheet".into(),
            a1: "A1".into(),
            source: "=1+2".into(),
        }),
        "committing the formula bar should tell the host the A1 source"
    );
    interact(&opened, cx, |window, _| {
        let displayed = window.find("cell-0-0");
        assert_eq!(
            displayed.value().or(displayed.label()),
            Some("3"),
            "A1 should display the calculated result 3; snapshot {displayed:?}"
        );
    });
}

#[gpui_kit::test]
fn clicking_another_cell_moves_the_selection_and_formula_bar(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    interact(&opened, cx, |window, cx| window.click("cell-0-0", cx));
    type_formula(&opened, cx, "=1+2");
    clear_events(&opened);
    interact(&opened, cx, |window, cx| window.click("cell-0-1", cx));
    assert_eq!(
        events(&opened),
        vec![SpreadsheetUiEvent::SelectionChanged {
            sheet: "Sheet".into(),
            a1: "B1".into(),
        }],
        "clicking B1 should notify the host of that selection"
    );
    interact(&opened, cx, |window, cx| {
        assert_eq!(
            opened
                .spreadsheet
                .read(cx)
                .session()
                .selection_a1()
                .as_deref(),
            Some("B1")
        );
        let formula = window.find(formula_bar_id(window));
        assert_eq!(
            formula.value(),
            Some(""),
            "the formula bar should follow B1, which is empty"
        );
    });
}

#[gpui_kit::test]
fn rejected_command_emits_command_failed_without_changing_the_document(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    let before = opened
        .spreadsheet
        .read_with(cx, |view, _| view.session().document().digest());
    clear_events(&opened);
    interact(&opened, cx, |window, cx| {
        opened.spreadsheet.update(cx, |view, cx| {
            view.apply_command(
                Command::AddSheet {
                    name: "Sheet".into(),
                },
                window,
                cx,
            )
        });
    });
    assert!(
        events(&opened).iter().any(|event| matches!(event,
            SpreadsheetUiEvent::CommandFailed { message } if message.contains("DuplicateName")
        )),
        "a duplicate sheet name should emit CommandFailed, got {:?}",
        events(&opened)
    );
    assert_eq!(
        opened
            .spreadsheet
            .read_with(cx, |view, _| view.session().document().digest()),
        before,
        "a rejected command must not change the document"
    );
}

#[gpui_kit::test]
fn committing_a_formula_with_no_selection_emits_command_failed(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    interact(&opened, cx, |window, cx| {
        let sheet = opened.spreadsheet.read(cx).session().document().sheets()[0];
        opened.spreadsheet.update(cx, |view, cx| {
            view.apply_command(Command::DeleteSheet { sheet }, window, cx)
        });
    });
    let before = opened
        .spreadsheet
        .read_with(cx, |view, _| view.session().document().digest());
    clear_events(&opened);
    type_formula(&opened, cx, "=1+2");
    assert!(
        events(&opened).iter().any(|event| matches!(event,
            SpreadsheetUiEvent::CommandFailed { message } if message.contains("no cell is selected")
        )),
        "enter with no selection should emit CommandFailed, got {:?}",
        events(&opened)
    );
    assert!(
        events(&opened)
            .iter()
            .all(|event| !matches!(event, SpreadsheetUiEvent::EditCommitted { .. })),
        "a rejected edit must not emit EditCommitted"
    );
    assert_eq!(
        opened
            .spreadsheet
            .read_with(cx, |view, _| view.session().document().digest()),
        before,
        "the rejected edit must not change the document"
    );
}

#[gpui_kit::test]
fn painted_fill_matches_projected_appearance(cx: &mut TestAppContext) {
    let path = write_fixture("generated");
    let opened = open_spreadsheet(cx);
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        opened.spreadsheet.update(cx, |view, cx| {
            view.load_appearance(&path, cx)
                .expect("generated workbook appearance");
        });
        window.render_frame(cx);
        assert_cell_fill_matches_projection(&path, &opened.spreadsheet, window, cx, 0, 0);

        let sample = Path::new(SAMPLE_53647);
        if !sample.is_file() {
            eprintln!("skipping spreadsheetbench sample; file is absent");
            return;
        }
        let window_spec = opened.spreadsheet.read(cx).session().visible_window();
        let projected = project_xlsx_appearance(sample, window_spec)
            .expect("project spreadsheetbench appearance");
        let Some(cell) = projected.colors.iter().find(|cell| cell.fill.is_some()) else {
            panic!("spreadsheetbench sample has no explicit rgb fill in the visible window");
        };
        let (row, column) = (cell.row, cell.column);
        opened.spreadsheet.update(cx, |view, cx| {
            view.load_appearance(sample, cx)
                .expect("spreadsheetbench appearance");
        });
        window.render_frame(cx);
        assert_cell_fill_matches_projection(sample, &opened.spreadsheet, window, cx, row, column);
    })
    .unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn independent_package_read_agrees_with_appearance_projection() {
    let path = write_fixture("package");
    let tile = project_xlsx_appearance(
        &path,
        VisibleWindow {
            origin_row: 0,
            origin_column: 0,
            rows: 32,
            columns: 12,
        },
    )
    .expect("project generated workbook");
    let package = read_package(&path);
    assert_eq!(
        tile.merges.len(),
        package.merges,
        "projection merge count should match the worksheet XML"
    );
    let fill = tile.fill(0, 0).expect("projected A1 fill");
    assert_eq!(
        fill, package.rgb,
        "projection should keep the explicit rgb fill from styles.xml"
    );
    let width = tile.width(0).expect("projected column A width");
    assert!(
        (width - package.width).abs() < 1e-9,
        "projection width {width} should match the worksheet custom width {}",
        package.width
    );
    assert!(
        (package.width - COLUMN_WIDTH).abs() < 1e-9,
        "fixture width should stay {}",
        COLUMN_WIDTH
    );
    assert!(
        tile.column_widths.iter().any(|column| column.column == 0
            && column.custom
            && (column.width - package.width).abs() < 1e-9),
        "column A should be reported as a custom width"
    );
    let _ = std::fs::remove_file(path);
}

fn assert_cell_fill_matches_projection(
    path: &Path,
    spreadsheet: &Entity<SpreadsheetView>,
    window: &Window,
    cx: &gpui_kit::App,
    row: u32,
    column: u32,
) {
    let projected = project_xlsx_appearance(path, spreadsheet.read(cx).session().visible_window())
        .expect("project appearance");
    let fill = projected
        .fill(row, column)
        .unwrap_or_else(|| panic!("no projected fill at row {row} column {column}"));
    let expected = gpui_kit::Hsla::from(gpui_kit::rgb(fill.packed()));
    let scale = window.scale_factor();
    let width = spreadsheet
        .read(cx)
        .session()
        .column_width_px(column as usize)
        * scale;
    let height = CELL_HEIGHT_PX * scale;
    let quads = window.painted_quads();
    let same_color: Vec<_> = quads
        .iter()
        .filter(|quad| quad.background.as_solid() == Some(expected))
        .map(|quad| quad.bounds.size)
        .collect();
    assert!(
        same_color.iter().any(|size| {
            (size.width.0 - width).abs() < 0.6 && (size.height.0 - height).abs() < 0.6
        }),
        "painted fill of cell {row},{column} should match projected {:#08x} at {width:.1}x{height:.1}; \
         frame has {} quads, same-color sizes {same_color:?}",
        fill.packed(),
        quads.len(),
    );
}

struct PackageFacts {
    merges: usize,
    rgb: RgbColor,
    width: f64,
}

fn read_package(path: &Path) -> PackageFacts {
    let bytes = std::fs::read(path).expect("read xlsx");
    let parts = stored_zip_entries(&bytes);
    let styles = part_text(&parts, "xl/styles.xml");
    let sheet = part_text(&parts, "xl/worksheets/sheet1.xml");
    let rgb_at = styles
        .find(&format!("rgb=\"{FILL_RGB}\""))
        .expect("styles.xml should carry the explicit rgb");
    let hex = &styles[rgb_at + 5..rgb_at + 5 + FILL_RGB.len()];
    assert_eq!(hex.len(), 8);
    let rgb = RgbColor {
        red: u8::from_str_radix(&hex[2..4], 16).unwrap(),
        green: u8::from_str_radix(&hex[4..6], 16).unwrap(),
        blue: u8::from_str_radix(&hex[6..8], 16).unwrap(),
    };
    let merges = sheet.matches("<mergeCell ").count();
    let width_at = sheet
        .find("width=\"")
        .expect("worksheet should set a column width");
    let width_end = sheet[width_at + 7..]
        .find('"')
        .expect("width attribute closes");
    let width: f64 = sheet[width_at + 7..width_at + 7 + width_end]
        .parse()
        .expect("width is a number");
    assert!(
        sheet.contains("customWidth=\"1\""),
        "worksheet should mark the width custom"
    );
    PackageFacts { merges, rgb, width }
}

fn write_fixture(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "omasheets-gpui-{name}-{}-{}.xlsx",
        std::process::id(),
        FILL_RGB
    ));
    std::fs::write(&path, workbook_bytes()).expect("write xlsx");
    path
}

fn workbook_bytes() -> Vec<u8> {
    // Stored zip so this test can read the package without the zip crate.
    let entries = [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/worksheets/sheet1.xml", SHEET),
    ];
    store_zip(&entries.map(|(name, text)| (name, text.as_bytes())))
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Sheet" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#;

const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="3">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="gray125"/></fill>
    <fill><patternFill patternType="solid"><fgColor rgb="FFC41E3A"/></patternFill></fill>
  </fills>
  <borders count="1"><border/></borders>
  <cellXfs count="2">
    <xf fontId="0" fillId="0" borderId="0"/>
    <xf fontId="0" fillId="2" borderId="0" applyFill="1"/>
  </cellXfs>
</styleSheet>"#;

const SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <cols><col min="1" max="1" width="20" customWidth="1"/></cols>
  <sheetData>
    <row r="1"><c r="A1" s="1"><v>1</v></c></row>
  </sheetData>
  <mergeCells count="1"><mergeCell ref="A1:B2"/></mergeCells>
</worksheet>"#;

fn store_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = body.len() as u32;
        let crc = crc32(data);
        let name_bytes = name.as_bytes();
        push_local(&mut body, name_bytes, data, crc);
        push_central(&mut central, name_bytes, data, crc, offset);
    }
    let central_offset = body.len() as u32;
    let central_len = central.len() as u32;
    body.extend_from_slice(&central);
    body.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    body.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    body.extend_from_slice(&central_len.to_le_bytes());
    body.extend_from_slice(&central_offset.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body
}

fn push_local(out: &mut Vec<u8>, name: &[u8], data: &[u8], crc: u32) {
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
}

fn push_central(out: &mut Vec<u8>, name: &[u8], data: &[u8], crc: u32, offset: u32) {
    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(name);
}

fn stored_zip_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let eocd = bytes
        .windows(4)
        .rposition(|marker| marker == [0x50, 0x4b, 0x05, 0x06])
        .expect("zip end of central directory");
    let count = u16::from_le_bytes(bytes[eocd + 10..eocd + 12].try_into().unwrap()) as usize;
    let cd_len = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
    let cd_off = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let directory = &bytes[cd_off..cd_off + cd_len];
    let mut cursor = 0;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        assert_eq!(&directory[cursor..cursor + 4], &[0x50, 0x4b, 0x01, 0x02]);
        let method = u16::from_le_bytes(directory[cursor + 10..cursor + 12].try_into().unwrap());
        assert_eq!(method, 0, "package reader only accepts stored entries");
        let size =
            u32::from_le_bytes(directory[cursor + 20..cursor + 24].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(directory[cursor + 28..cursor + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(directory[cursor + 30..cursor + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(directory[cursor + 32..cursor + 34].try_into().unwrap()) as usize;
        let local_off =
            u32::from_le_bytes(directory[cursor + 42..cursor + 46].try_into().unwrap()) as usize;
        let name = String::from_utf8(directory[cursor + 46..cursor + 46 + name_len].to_vec())
            .expect("zip entry name");
        cursor += 46 + name_len + extra_len + comment_len;
        let local = &bytes[local_off..];
        assert_eq!(&local[..4], &[0x50, 0x4b, 0x03, 0x04]);
        let local_name = u16::from_le_bytes(local[26..28].try_into().unwrap()) as usize;
        let local_extra = u16::from_le_bytes(local[28..30].try_into().unwrap()) as usize;
        let start = 30 + local_name + local_extra;
        entries.push((name, local[start..start + size].to_vec()));
    }
    entries
}

fn part_text(parts: &[(String, Vec<u8>)], name: &str) -> String {
    let (_name, bytes) = parts
        .iter()
        .find(|(part, _)| part == name)
        .unwrap_or_else(|| panic!("missing {name}"));
    String::from_utf8(bytes.clone()).expect("xml is utf-8")
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// A spreadsheet embedded in a note must not pull the keyboard out of the
/// paragraph being typed in when it finishes opening; a standalone File page
/// still focuses its grid.
#[gpui_kit::test]
fn autofocus_off_leaves_focus_where_it_was(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let focused = |autofocus: bool, cx: &mut TestAppContext| {
        let window = cx.open_window(size(px(800.), px(600.)), move |window, cx| {
            let spreadsheet = cx.new(|cx| {
                let mut view = SpreadsheetView::new(
                    SpreadsheetSession::open("book").expect("session opens"),
                    window,
                    cx,
                );
                view.set_autofocus(autofocus);
                view
            });
            let host = cx.new(|cx| Host::new(spreadsheet, Rc::default(), cx));
            Root::new(host, window, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| window.focused(cx).is_some())
            .expect("window open")
    };
    assert!(focused(true, cx), "a File page focuses its grid");
    assert!(!focused(false, cx), "an embed does not take focus on open");
}

#[gpui_kit::test]
fn typing_in_a_cell_and_pressing_enter_moves_one_row_down(cx: &mut TestAppContext) {
    // A spreadsheet user types down a column: each Enter must land on the
    // next row, or every other row is skipped.
    let opened = open_spreadsheet(cx);
    interact(&opened, cx, |window, cx| window.click("cell-0-0", cx));
    interact(&opened, cx, |window, cx| window.press("7", cx));
    interact(&opened, cx, |window, cx| window.press("enter", cx));
    interact(&opened, cx, |window, cx| {
        assert_eq!(
            opened
                .spreadsheet
                .read(cx)
                .session()
                .selection_a1()
                .as_deref(),
            Some("A2"),
            "one Enter commits A1 and selects the cell directly below"
        );
        let displayed = window.find("cell-0-0");
        assert_eq!(displayed.value().or(displayed.label()), Some("7"));
    });
}

/// An embed in a note on the phone: `width` points wide, with `setup`
/// applied to the view before the first frame.
fn open_embed(
    cx: &mut TestAppContext,
    width: f32,
    setup: impl FnOnce(&mut SpreadsheetView, &mut Context<SpreadsheetView>) + 'static,
) -> Opened {
    cx.update(gpui_kit::init);
    let events = Rc::new(RefCell::new(Vec::new()));
    let sink = events.clone();
    let slot: Rc<RefCell<Option<Entity<SpreadsheetView>>>> = Rc::new(RefCell::new(None));
    let slot_for_open = slot.clone();
    let host_presses: Rc<std::cell::Cell<usize>> = Rc::default();
    let presses_for_host = host_presses.clone();
    let window = cx.open_window(size(px(width), px(700.)), move |window, cx| {
        let spreadsheet = cx.new(|cx| {
            let mut view = SpreadsheetView::new(
                SpreadsheetSession::open("book").expect("session opens"),
                window,
                cx,
            );
            setup(&mut view, cx);
            view
        });
        slot_for_open.borrow_mut().replace(spreadsheet.clone());
        let host = cx.new(|cx| {
            let sink = sink.clone();
            let events = cx.subscribe(&spreadsheet, move |_host, _view, event, _cx| {
                sink.borrow_mut().push(event.clone());
            });
            let notified = cx.observe(&spreadsheet, |_host, _view, cx| cx.notify());
            EmbedHost {
                spreadsheet,
                presses: presses_for_host,
                _subscriptions: vec![events, notified],
            }
        });
        Root::new(host, window, cx)
    });
    // GPUI delivers focus and blur callbacks only to an active window.
    cx.update_window(window.into(), |_, window, _| window.activate_window())
        .expect("window open");
    cx.run_until_parked();
    Opened {
        window,
        spreadsheet: slot.borrow().clone().expect("spreadsheet mounted"),
        events,
        host_presses,
    }
}

/// Writes `source` into a cell through the session, as a host restore would.
fn put(view: &mut SpreadsheetView, row: usize, column: usize, source: &str) {
    let session = view.session_mut();
    session.select(row, column).expect("cell in view");
    session.set_formula_draft(source.to_string());
    session.commit_edit().expect("edit commits");
}

fn cell_text(opened: &Opened, cx: &mut TestAppContext, id: &str) -> String {
    let mut text = String::new();
    interact(opened, cx, |window, _cx| {
        let cell = window.find(id.to_string());
        text = cell
            .value()
            .or(cell.label())
            .unwrap_or_default()
            .to_string();
    });
    text
}

fn formula_focused(opened: &Opened, cx: &mut TestAppContext) -> bool {
    let mut focused = false;
    interact(opened, cx, |window, _cx| {
        let id = formula_bar_id(window);
        focused = window.find(id).focused() == Some(true);
    });
    focused
}

/// On a 390 pt phone the grid is wider than the note. The owner saw columns
/// drift from their headers and "343" painted as "43": cells shrank to fit.
/// Every column keeps its width, so headers line up and numbers paint whole.
#[gpui_kit::test]
fn a_narrow_embed_keeps_columns_aligned_and_numbers_whole(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, _cx| {
        put(view, 0, 0, "343");
        put(view, 0, 2, "343");
    });
    interact(&opened, cx, |window, cx| {
        let session = opened.spreadsheet.read(cx).session();
        let mut checked = 0;
        for column in 0..12 {
            let header = window.find(format!("column-header-{column}")).bounds();
            if header.origin.x >= px(390.) {
                break;
            }
            checked += 1;
            let width = session.column_width_px(column);
            assert!(
                (header.size.width.as_f32() - width).abs() < 0.5,
                "column {column} header keeps its {width} px width, got {:?}",
                header.size.width
            );
            for row in [0, 1, 5] {
                let cell = window.find(format!("cell-{row}-{column}")).bounds();
                assert_eq!(
                    cell.origin.x, header.origin.x,
                    "cell {row},{column} sits under its column header"
                );
                assert_eq!(cell.size.width, header.size.width);
            }
        }
        assert!(
            checked >= 4,
            "at least four columns are in view, got {checked}"
        );
        for column in [0, 2] {
            let cell = window.find(format!("cell-0-{column}")).bounds();
            let text = window.find(format!("cell-text-0-{column}")).bounds();
            assert!(
                text.origin.x >= cell.origin.x && text.right() <= cell.right(),
                "343 in column {column} paints inside its cell: text {text:?}, cell {cell:?}"
            );
        }
    });
}

/// A note shows a small table at its size and a long one in a window that
/// scrolls, never a fixed block clipped at row 8.
#[gpui_kit::test]
fn embed_height_fits_used_rows_between_five_and_fifteen(_cx: &mut TestAppContext) {
    let mut session = SpreadsheetSession::open("book").expect("session opens");
    assert_eq!(session.embed_rows(), 5, "an empty sheet still shows 5 rows");
    for row in 0..3 {
        session.select(row, 0).expect("cell");
        session.set_formula_draft(format!("{row}"));
        session.commit_edit().expect("commit");
    }
    assert_eq!(session.used_rows(), 3);
    assert_eq!(
        session.embed_rows(),
        5,
        "3 used rows plus one empty, at least 5"
    );
    for row in 0..40 {
        session.select(row, 1).expect("cell");
        session.set_formula_draft(format!("{row}"));
        session.commit_edit().expect("commit");
    }
    assert_eq!(session.used_rows(), 40);
    assert_eq!(
        session.embed_rows(),
        15,
        "a long sheet stops at 15 rows and scrolls"
    );
}

#[gpui_kit::test]
fn embed_height_counts_chrome_and_row_heights(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, _cx| {
        for row in 0..3 {
            put(view, row, 0, "1");
        }
    });
    let height = cx.update(|cx| opened.spreadsheet.read(cx).embed_height_px());
    // Formula bar 32, sheet tabs 28, column header 22, five 22 px rows.
    assert_eq!(height, 32.0 + 28.0 + 22.0 + 5.0 * 22.0);
}

/// Phone gestures from Numbers and Google Sheets: tapping around a sheet
/// must not raise the keyboard; a double-tap edits; Return walks down a column.
#[gpui_kit::test]
fn touch_tap_selects_and_double_tap_edits(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, cx| {
        view.set_autofocus(false);
        view.set_touch(true, cx);
        put(view, 0, 0, "12");
        put(view, 1, 1, "5");
    });
    interact(&opened, cx, |window, cx| window.click("cell-1-1", cx));
    let state = cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (view.session().selection_a1(), view.editing())
    });
    assert_eq!(state, (Some("B2".to_string()), false), "a tap only selects");
    assert!(
        !formula_focused(&opened, cx),
        "a tap leaves the keyboard down"
    );

    interact(&opened, cx, |window, cx| {
        window.double_click("cell-0-0", cx)
    });
    assert!(formula_focused(&opened, cx), "a double-tap edits the cell");
    let draft = cx.update(|cx| {
        opened
            .spreadsheet
            .read(cx)
            .session()
            .formula_draft()
            .to_string()
    });
    assert_eq!(draft, "12", "the edit starts from the cell's contents");

    interact(&opened, cx, |window, cx| window.input("3", cx));
    interact(&opened, cx, |window, cx| window.press("enter", cx));
    assert_eq!(cell_text(&opened, cx, "cell-0-0"), "123", "Return commits");
    let state = cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (view.session().selection_a1(), view.editing())
    });
    assert_eq!(
        state,
        (Some("A2".to_string()), true),
        "and edits the cell below"
    );
    assert!(formula_focused(&opened, cx), "the keyboard stays up");

    interact(&opened, cx, |window, cx| window.input("7", cx));
    interact(&opened, cx, |window, cx| {
        window.click("omasheets-formula-commit", cx)
    });
    assert_eq!(cell_text(&opened, cx, "cell-1-0"), "7", "✓ commits");
    let state = cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (view.session().selection_a1(), view.editing())
    });
    assert_eq!(
        state,
        (Some("A2".to_string()), false),
        "✓ ends the edit on the same cell"
    );
    assert!(!formula_focused(&opened, cx));
    let shown = cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find("omasheets-formula-commit").is_some()
    });
    assert_eq!(shown.ok(), Some(false), "✓ and ✕ show only while editing");

    interact(&opened, cx, |window, cx| {
        window.double_click("cell-2-0", cx)
    });
    interact(&opened, cx, |window, cx| window.input("9", cx));
    interact(&opened, cx, |window, cx| {
        window.click("omasheets-formula-discard", cx)
    });
    assert_eq!(cell_text(&opened, cx, "cell-2-0"), "", "✕ discards");
    let state = cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (view.session().selection_a1(), view.editing())
    });
    assert_eq!(
        state,
        (Some("A3".to_string()), false),
        "✕ ends the edit on the same cell"
    );

    // A tap on the formula field edits the selected cell too.
    interact(&opened, cx, |window, cx| {
        window.click(formula_bar_id(window), cx)
    });
    assert!(
        formula_focused(&opened, cx),
        "a tap on the formula field edits"
    );
    let editing = cx.update(|cx| opened.spreadsheet.read(cx).editing());
    assert!(editing, "the field shows ✓ and ✕ while editing");
    interact(&opened, cx, |window, cx| {
        window.click("omasheets-formula-discard", cx)
    });

    // Done on the keyboard, or a tap outside the field, blurs it: that commits.
    interact(&opened, cx, |window, cx| {
        window.double_click("cell-2-0", cx)
    });
    interact(&opened, cx, |window, cx| window.input("4", cx));
    interact(&opened, cx, |window, cx| window.blur(cx));
    assert_eq!(cell_text(&opened, cx, "cell-2-0"), "4", "blur commits");
    let editing = cx.update(|cx| opened.spreadsheet.read(cx).editing());
    assert!(!editing, "blur ends the edit");
}

/// A read-only embed (Ashlar's phone before editing is allowed) used to take
/// typing and then refuse Enter silently. Its formula field only shows.
#[gpui_kit::test]
fn read_only_formula_field_takes_no_focus_or_typing(cx: &mut TestAppContext) {
    for touch in [false, true] {
        let opened = open_embed(cx, 390.0, move |view, cx| {
            view.set_autofocus(false);
            view.set_touch(touch, cx);
            put(view, 0, 0, "12");
            view.set_readonly(true, cx);
        });
        interact(&opened, cx, |window, cx| window.click("cell-0-0", cx));
        interact(&opened, cx, |window, cx| {
            window.click(formula_bar_id(window), cx)
        });
        assert!(
            !formula_focused(&opened, cx),
            "touch={touch}: a click does not focus it"
        );
        interact(&opened, cx, |window, cx| window.input("9", cx));
        interact(&opened, cx, |window, cx| {
            window.double_click("cell-0-0", cx)
        });
        interact(&opened, cx, |window, cx| window.input("8", cx));
        assert!(
            !formula_focused(&opened, cx),
            "touch={touch}: a double-tap does not edit"
        );
        let (draft, editing) = cx.update(|cx| {
            let view = opened.spreadsheet.read(cx);
            (view.session().formula_draft().to_string(), view.editing())
        });
        assert_eq!(
            draft, "12",
            "touch={touch}: the field shows the cell and typing changes nothing"
        );
        assert!(!editing);
        assert_eq!(cell_text(&opened, cx, "cell-0-0"), "12");
    }
}

fn selection(opened: &Opened, cx: &mut TestAppContext) -> (Option<String>, bool) {
    cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (view.session().selection_a1(), view.editing())
    })
}

/// The block editor marks an embed's block from its own press handler, so
/// Escape returns the caret to the embed that was clicked. A cell press that
/// stopped there left the caret in the previous block.
#[gpui_kit::test]
fn a_cell_press_still_reaches_the_host(cx: &mut TestAppContext) {
    for touch in [false, true] {
        let opened = open_embed(cx, 390.0, move |view, cx| {
            view.set_autofocus(false);
            view.set_touch(touch, cx);
        });
        interact(&opened, cx, |window, cx| window.click("cell-1-1", cx));
        assert_eq!(
            opened.host_presses.get(),
            1,
            "touch={touch}: the host sees the press"
        );
        assert_eq!(selection(&opened, cx).0.as_deref(), Some("B2"));
        if touch {
            interact(&opened, cx, |window, cx| {
                window.double_click("cell-1-1", cx)
            });
            assert!(
                formula_focused(&opened, cx),
                "the grid's press must not take focus back from a double-tap edit"
            );
        } else {
            interact(&opened, cx, |window, cx| window.press("7", cx));
            interact(&opened, cx, |window, cx| window.press("enter", cx));
            assert_eq!(
                cell_text(&opened, cx, "cell-1-1"),
                "7",
                "the grid has the keyboard"
            );
        }
    }
}

/// Switching sheets mid-edit used to load the new sheet's A1 into the field,
/// and the late blur then wrote it into that sheet, losing the edit.
#[gpui_kit::test]
fn switching_sheets_mid_edit_commits_to_the_edited_sheet(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, cx| {
        view.set_autofocus(false);
        view.set_touch(true, cx);
        view.session_mut()
            .apply_command(Command::AddSheet { name: "Two".into() })
            .expect("second sheet");
    });
    interact(&opened, cx, |window, cx| {
        window.double_click("cell-2-1", cx)
    });
    interact(&opened, cx, |window, cx| window.input("42", cx));
    interact(&opened, cx, |window, cx| window.click("sheet-tab-1", cx));
    let (on_two, editing) = cx.update(|cx| {
        let view = opened.spreadsheet.read(cx);
        (
            view.session().active_sheet_name().map(str::to_string),
            view.editing(),
        )
    });
    assert_eq!(on_two.as_deref(), Some("Two"));
    assert!(!editing, "the switch ends the edit");
    assert_eq!(
        cell_text(&opened, cx, "cell-0-0"),
        "",
        "nothing lands in Two!A1"
    );
    assert_eq!(cell_text(&opened, cx, "cell-2-1"), "", "nor in Two!B3");
    interact(&opened, cx, |window, cx| window.click("sheet-tab-0", cx));
    assert_eq!(
        cell_text(&opened, cx, "cell-2-1"),
        "42",
        "the edit landed in the first sheet"
    );
}

/// Return walks down a column; the cell being edited must stay in the
/// 5-15 rows an embed shows, and keys walking right stay in the columns
/// that fit a 390 pt phone.
#[gpui_kit::test]
fn the_edited_cell_stays_in_the_painted_embed(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, cx| {
        view.set_autofocus(false);
        view.set_touch(true, cx);
    });
    interact(&opened, cx, |window, cx| {
        window.double_click("cell-0-0", cx)
    });
    for _ in 0..20 {
        interact(&opened, cx, |window, cx| window.input("1", cx));
        interact(&opened, cx, |window, cx| window.press("enter", cx));
    }
    assert_eq!(selection(&opened, cx), (Some("A21".to_string()), true));
    let height = cx.update(|cx| opened.spreadsheet.read(cx).embed_height_px());
    interact(&opened, cx, |window, _cx| {
        let cell = window.find("cell-20-0").bounds();
        assert!(
            cell.origin.y >= px(0.) && cell.bottom() <= px(height),
            "A21 {cell:?} is inside the {height} px embed"
        );
    });
    interact(&opened, cx, |window, cx| {
        window.click("omasheets-formula-commit", cx)
    });
    for _ in 0..6 {
        interact(&opened, cx, |window, cx| window.press("right", cx));
    }
    assert_eq!(selection(&opened, cx).0.as_deref(), Some("G21"));
    interact(&opened, cx, |window, _cx| {
        let cell = window.find("cell-20-6").bounds();
        assert!(
            cell.origin.x >= px(48.) && cell.right() <= px(390.),
            "G21 {cell:?} is inside the 390 px embed"
        );
    });
}

/// ✓ on a formula the engine refuses keeps the edit, as Enter does on the
/// desktop, so the user can correct it instead of losing it.
#[gpui_kit::test]
fn a_refused_commit_keeps_the_edit(cx: &mut TestAppContext) {
    let opened = open_embed(cx, 390.0, |view, cx| {
        view.set_autofocus(false);
        view.set_touch(true, cx);
    });
    interact(&opened, cx, |window, cx| {
        window.double_click("cell-0-0", cx)
    });
    interact(&opened, cx, |window, cx| window.input(REFUSED, cx));
    clear_events(&opened);
    interact(&opened, cx, |window, cx| {
        window.click("omasheets-formula-commit", cx)
    });
    assert!(
        events(&opened)
            .iter()
            .any(|event| matches!(event, SpreadsheetUiEvent::CommandFailed { .. })),
        "the formula is refused: {:?}",
        events(&opened)
    );
    assert_eq!(
        selection(&opened, cx),
        (Some("A1".to_string()), true),
        "still editing"
    );
    assert!(formula_focused(&opened, cx), "the keyboard stays up");
    let draft = cx.update(|cx| {
        opened
            .spreadsheet
            .read(cx)
            .session()
            .formula_draft()
            .to_string()
    });
    assert_eq!(draft, REFUSED, "the draft is kept");
}

const REFUSED: &str = "=1+";
