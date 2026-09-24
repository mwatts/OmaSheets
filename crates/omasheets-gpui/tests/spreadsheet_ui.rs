//! Headless checks of the production [`SpreadsheetView`].
//! The view is not reimplemented here.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    TestAppContext, Window, WindowHandle, div, px, size,
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
        Root::new(host, window, cx).bordered(false)
    });
    Opened {
        window,
        spreadsheet: slot.borrow().clone().expect("spreadsheet mounted"),
        events,
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

fn type_formula(window: &mut Window, cx: &mut gpui_kit::App, source: &str) {
    let id = formula_bar_id(window);
    window.click(id, cx);
    window.input(source, cx);
    window.press("enter", cx);
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
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("cell-0-0", cx);
        type_formula(window, cx, "=1+2");

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

        let displayed = window.find("cell-0-0");
        let model = opened
            .spreadsheet
            .read(cx)
            .session()
            .visible_cells()
            .into_iter()
            .find(|cell| cell.row == 0 && cell.column == 0)
            .map(|cell| cell.text);
        assert_eq!(
            displayed.value().or(displayed.label()),
            Some("3"),
            "A1 should display the calculated result 3; model text is {model:?}; snapshot {displayed:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn clicking_another_cell_moves_the_selection_and_formula_bar(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("cell-0-0", cx);
        type_formula(window, cx, "=1+2");
        clear_events(&opened);
        window.click("cell-0-1", cx);

        assert_eq!(
            events(&opened),
            vec![SpreadsheetUiEvent::SelectionChanged {
                sheet: "Sheet".into(),
                a1: "B1".into(),
            }],
            "clicking B1 should notify the host of that selection"
        );
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
            "the formula bar should follow B1, which is empty, not keep A1's formula; snapshot {formula:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn rejected_command_emits_command_failed_without_changing_the_document(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let before = opened.spreadsheet.read(cx).session().document().digest();
        clear_events(&opened);
        opened.spreadsheet.update(cx, |view, cx| {
            view.apply_command(
                Command::AddSheet {
                    name: "Sheet".into(),
                },
                window,
                cx,
            );
        });
        assert!(
            events(&opened).iter().any(|event| matches!(
                event,
                SpreadsheetUiEvent::CommandFailed { message }
                    if message.contains("DuplicateName")
            )),
            "a duplicate sheet name should emit CommandFailed, got {:?}",
            events(&opened)
        );
        assert_eq!(
            opened.spreadsheet.read(cx).session().document().digest(),
            before,
            "a rejected command must not change the document"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn committing_a_formula_with_no_selection_emits_command_failed(cx: &mut TestAppContext) {
    let opened = open_spreadsheet(cx);
    cx.update_window(opened.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let sheet = opened.spreadsheet.read(cx).session().document().sheets()[0];
        opened.spreadsheet.update(cx, |view, cx| {
            view.apply_command(Command::DeleteSheet { sheet }, window, cx);
        });
        let before = opened.spreadsheet.read(cx).session().document().digest();
        clear_events(&opened);
        type_formula(window, cx, "=1+2");

        assert!(
            events(&opened).iter().any(|event| matches!(
                event,
                SpreadsheetUiEvent::CommandFailed { message }
                    if message.contains("no cell is selected")
            )),
            "enter in the formula bar with no selection should emit CommandFailed, got {:?}",
            events(&opened)
        );
        assert!(
            events(&opened)
                .iter()
                .all(|event| !matches!(event, SpreadsheetUiEvent::EditCommitted { .. })),
            "a rejected edit must not emit EditCommitted"
        );
        assert_eq!(
            opened.spreadsheet.read(cx).session().document().digest(),
            before,
            "the rejected edit must not change the document"
        );
    })
    .unwrap();
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
