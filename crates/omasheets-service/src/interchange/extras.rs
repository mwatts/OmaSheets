//! Excel projections of native presentation. Only package-local relationships
//! are read; unsupported shapes are disclosed instead of approximated silently.
use super::*;
use omasheets_core::presentation::{ChartKind, Comparison};
use omasheets_core::{CellRef, CellValue};
use std::fmt::Write as _;

const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

#[derive(Default)]
pub(crate) struct Export {
    pub content_types: String,
    pub parts: Vec<(String, String)>,
    pub filters: Vec<String>,
    pub suffixes: Vec<String>,
    pub hidden_rows: Vec<BTreeSet<usize>>,
    pub dxfs: String,
    pub losses: Vec<String>,
}

fn range(document: &Document, sheet: SheetId, region: &Region) -> Result<String, ServiceError> {
    let first = document
        .project_a1(CellRef {
            sheet,
            row: region.rows[0],
            column: region.columns[0],
        })
        .ok_or_else(|| invalid("Missing presentation anchor"))?;
    let last = document
        .project_a1(CellRef {
            sheet,
            row: *region.rows.last().unwrap(),
            column: *region.columns.last().unwrap(),
        })
        .ok_or_else(|| invalid("Missing presentation anchor"))?;
    Ok(format!("{first}:{last}"))
}
fn plain(value: CellValue) -> String {
    match value {
        CellValue::Blank => String::new(),
        CellValue::Number(n) => n.to_string(),
        CellValue::Text(t) | CellValue::Error(t) => t,
        CellValue::Boolean(b) => if b { "TRUE" } else { "FALSE" }.into(),
    }
}
fn dxf(style: &CellStyle) -> Result<String, ServiceError> {
    let mut out = String::from("<dxf><font>");
    if style.bold {
        out.push_str("<b/>");
    }
    if style.italic {
        out.push_str("<i/>");
    }
    if style.underline {
        out.push_str("<u/>");
    }
    if let Some(size) = style.font_size {
        write!(out, "<sz val=\"{size}\"/>").unwrap();
    }
    if let Some(color) = &style.foreground {
        write!(out, "<color rgb=\"FF{}\"/>", &color[1..]).unwrap();
    }
    out.push_str("</font>");
    if let Some(color) = &style.background {
        write!(
            out,
            "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF{}\"/></patternFill></fill>",
            &color[1..]
        )
        .unwrap();
    }
    if !style.number_format.is_empty() {
        write!(
            out,
            "<numFmt numFmtId=\"164\" formatCode=\"{}\"/>",
            xml_text(&style.number_format)?
        )
        .unwrap();
    }
    write!(
        out,
        "<alignment horizontal=\"{}\" wrapText=\"{}\"/>",
        match style.alignment {
            Alignment::General => "general",
            Alignment::Left => "left",
            Alignment::Center => "center",
            Alignment::Right => "right",
        },
        u8::from(style.wrap)
    )
    .unwrap();
    if style.border != Border::None {
        out.push_str("<border>");
        for side in ["left", "right", "top", "bottom"] {
            if style.border == Border::All || side == "bottom" {
                write!(out, "<{side} style=\"thin\"/>").unwrap();
            }
        }
        out.push_str("</border>");
    }
    out.push_str("</dxf>");
    Ok(out)
}

pub(crate) fn export(document: &Document) -> Result<Export, ServiceError> {
    let mut result = Export::default();
    let mut dxfs = String::new();
    let mut dxf_count = 0;
    let mut chart_count = 0;
    for (si, sheet) in document.sheets().iter().enumerate() {
        let n = si + 1;
        let presentation = document.presentation(*sheet).map_err(invalid)?;
        let mut rels = String::new();
        let mut suffix = String::new();
        let mut filter_xml = String::new();
        let mut hidden = BTreeSet::new();
        if let Some(filter) = &presentation.filter {
            let needle = if filter.case_sensitive {
                filter.text.clone()
            } else {
                filter.text.to_lowercase()
            };
            for row in filter.range.rows.iter().skip(usize::from(filter.header)) {
                let value = plain(document.value(CellRef {
                    sheet: *sheet,
                    row: *row,
                    column: filter.column,
                }));
                let value = if filter.case_sensitive {
                    value
                } else {
                    value.to_lowercase()
                };
                if !value.contains(&needle) {
                    hidden.insert(
                        document
                            .rows(*sheet)
                            .unwrap()
                            .iter()
                            .position(|r| r == row)
                            .unwrap(),
                    );
                }
            }
            if filter.header && !filter.case_sensitive {
                let pattern = format!(
                    "*{}*",
                    filter
                        .text
                        .replace('~', "~~")
                        .replace('*', "~*")
                        .replace('?', "~?")
                );
                let col = filter
                    .range
                    .columns
                    .iter()
                    .position(|c| *c == filter.column)
                    .unwrap();
                write!(filter_xml,"<autoFilter ref=\"{}\"><filterColumn colId=\"{col}\"><customFilters><customFilter operator=\"equal\" val=\"{}\"/></customFilters></filterColumn></autoFilter>",range(document,*sheet,&filter.range)?,xml_text(&pattern)?).unwrap();
            } else {
                result.losses.push("Case-sensitive or headerless filter criteria cannot be represented by Excel AutoFilter; current hidden rows are preserved.".into());
            }
        }
        for (priority, rule) in presentation.conditional.iter().enumerate() {
            dxfs.push_str(&dxf(&rule.style)?);
            write!(suffix,"<conditionalFormatting sqref=\"{}\"><cfRule type=\"cellIs\" dxfId=\"{dxf_count}\" priority=\"{}\" operator=\"{}\"><formula>{}</formula></cfRule></conditionalFormatting>",range(document,*sheet,&rule.range)?,presentation.conditional.len()-priority,match rule.comparison {Comparison::Greater=>"greaterThan",Comparison::Less=>"lessThan",Comparison::Equal=>"equal"},rule.value).unwrap();
            dxf_count += 1;
        }
        let notes: Vec<_> = presentation
            .cells
            .iter()
            .filter(|c| !c.note.is_empty())
            .collect();
        if !notes.is_empty() {
            let mut comments = format!(
                "<comments xmlns=\"{MAIN}\"><authors><author>OmaSheets</author></authors><commentList>"
            );
            let mut vml = String::from(
                "<xml xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:o=\"urn:schemas-microsoft-com:office:office\" xmlns:x=\"urn:schemas-microsoft-com:office:excel\"><v:shapetype id=\"_x0000_t202\" coordsize=\"21600,21600\" o:spt=\"202\" path=\"m,l,21600r21600,l21600,xe\"><v:stroke joinstyle=\"miter\"/><v:path gradientshapeok=\"t\" o:connecttype=\"rect\"/></v:shapetype>",
            );
            for (i, cell) in notes.iter().enumerate() {
                let address = document
                    .project_a1(CellRef {
                        sheet: *sheet,
                        row: cell.row,
                        column: cell.column,
                    })
                    .unwrap();
                write!(comments,"<comment ref=\"{address}\" authorId=\"0\"><text><t xml:space=\"preserve\">{}</t></text></comment>",xml_text(&cell.note)?).unwrap();
                let (r, c) = super::address(&address)?;
                write!(vml,"<v:shape id=\"_x0000_s{}\" type=\"#_x0000_t202\" style=\"position:absolute;width:144pt;height:79pt;visibility:hidden\" fillcolor=\"#ffffe1\" o:insetmode=\"auto\"><v:textbox><div/></v:textbox><x:ClientData ObjectType=\"Note\"><x:MoveWithCells/><x:SizeWithCells/><x:Row>{r}</x:Row><x:Column>{c}</x:Column></x:ClientData></v:shape>",1025+i).unwrap();
            }
            comments.push_str("</commentList></comments>");
            vml.push_str("</xml>");
            result.parts.push((format!("xl/comments{n}.xml"), comments));
            result
                .parts
                .push((format!("xl/drawings/comments{n}.vml"), vml));
            write!(result.content_types,"<Override PartName=\"/xl/comments{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml\"/><Override PartName=\"/xl/drawings/comments{n}.vml\" ContentType=\"application/vnd.openxmlformats-officedocument.vmlDrawing\"/>").unwrap();
            write!(rels,"<Relationship Id=\"notes\" Type=\"{REL}/comments\" Target=\"../comments{n}.xml\"/><Relationship Id=\"noteShapes\" Type=\"{REL}/vmlDrawing\" Target=\"../drawings/comments{n}.vml\"/>").unwrap();
        }
        if !presentation.charts.is_empty() {
            let mut drawing = format!(
                "<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"{REL}\">"
            );
            let mut drawing_rels = String::new();
            for (i, chart) in presentation.charts.iter().enumerate() {
                chart_count += 1;
                let id = chart_count;
                result.parts.push((
                    format!("xl/charts/chart{id}.xml"),
                    export_chart(document, *sheet, chart)?,
                ));
                write!(result.content_types,"<Override PartName=\"/xl/charts/chart{id}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/>").unwrap();
                write!(drawing_rels,"<Relationship Id=\"chart{id}\" Type=\"{REL}/chart\" Target=\"../charts/chart{id}.xml\"/>").unwrap();
                write!(drawing,"<xdr:oneCellAnchor><xdr:from><xdr:col>9</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:ext cx=\"6096000\" cy=\"3429000\"/><xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr><xdr:cNvPr id=\"{id}\" name=\"Chart {id}\"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" r:id=\"chart{id}\"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:oneCellAnchor>",i*18).unwrap();
            }
            drawing.push_str("</xdr:wsDr>");
            result
                .parts
                .push((format!("xl/drawings/drawing{n}.xml"), drawing));
            result.parts.push((
                format!("xl/drawings/_rels/drawing{n}.xml.rels"),
                relationships(&drawing_rels),
            ));
            write!(result.content_types,"<Override PartName=\"/xl/drawings/drawing{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawing+xml\"/>").unwrap();
            write!(rels,"<Relationship Id=\"drawing\" Type=\"{REL}/drawing\" Target=\"../drawings/drawing{n}.xml\"/>").unwrap();
            suffix.push_str("<drawing r:id=\"drawing\"/>");
        }
        if !notes.is_empty() {
            suffix.push_str("<legacyDrawing r:id=\"noteShapes\"/>");
        }
        if !rels.is_empty() {
            result.parts.push((
                format!("xl/worksheets/_rels/sheet{n}.xml.rels"),
                relationships(&rels),
            ));
        }
        result.filters.push(filter_xml);
        result.suffixes.push(suffix);
        result.hidden_rows.push(hidden);
    }
    result.dxfs = format!("<dxfs count=\"{dxf_count}\">{dxfs}</dxfs>");
    Ok(result)
}
fn relationships(body: &str) -> String {
    format!(
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>"
    )
}

fn export_chart(
    document: &Document,
    sheet: SheetId,
    chart: &omasheets_core::presentation::Chart,
) -> Result<String, ServiceError> {
    let mut out = format!(
        "<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><c:chart><c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:layout/>",
        xml_text(&chart.title)?
    );
    let tag = match chart.kind {
        ChartKind::Bar => "barChart",
        ChartKind::Line => "lineChart",
        ChartKind::Pie => "pieChart",
    };
    write!(out, "<c:{tag}>").unwrap();
    if chart.kind == ChartKind::Bar {
        out.push_str("<c:barDir val=\"col\"/><c:grouping val=\"clustered\"/>");
    }
    if chart.kind == ChartKind::Line {
        out.push_str("<c:grouping val=\"standard\"/>");
    }
    let prefix = format!(
        "'{}'!",
        document.sheet_name(sheet).unwrap().replace('\'', "''")
    );
    let address = |row, column| document.project_a1(CellRef { sheet, row, column }).unwrap();
    for (i, column) in chart.range.columns.iter().skip(1).enumerate() {
        let first = chart.range.rows[1];
        let last = *chart.range.rows.last().unwrap();
        let category = chart.range.columns[0];
        write!(out,"<c:ser><c:idx val=\"{i}\"/><c:order val=\"{i}\"/><c:tx><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val=\"1\"/><c:pt idx=\"0\"><c:v>{}</c:v></c:pt></c:strCache></c:strRef></c:tx>",xml_text(&format!("{prefix}{}",address(chart.range.rows[0],*column)))?,xml_text(&plain(document.value(CellRef{sheet,row:chart.range.rows[0],column:*column})))?).unwrap();
        write!(
            out,
            "<c:cat><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val=\"{}\"/>",
            xml_text(&format!(
                "{prefix}{}:{}",
                address(first, category),
                address(last, category)
            ))?,
            chart.range.rows.len() - 1
        )
        .unwrap();
        for (j, row) in chart.range.rows.iter().skip(1).enumerate() {
            write!(
                out,
                "<c:pt idx=\"{j}\"><c:v>{}</c:v></c:pt>",
                xml_text(&plain(document.value(CellRef {
                    sheet,
                    row: *row,
                    column: category
                })))?
            )
            .unwrap();
        }
        write!(out,"</c:strCache></c:strRef></c:cat><c:val><c:numRef><c:f>{}</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val=\"{}\"/>",xml_text(&format!("{prefix}{}:{}",address(first,*column),address(last,*column)))?,chart.range.rows.len()-1).unwrap();
        for (j, row) in chart.range.rows.iter().skip(1).enumerate() {
            if let CellValue::Number(v) = document.value(CellRef {
                sheet,
                row: *row,
                column: *column,
            }) {
                write!(out, "<c:pt idx=\"{j}\"><c:v>{v}</c:v></c:pt>").unwrap();
            }
        }
        out.push_str("</c:numCache></c:numRef></c:val></c:ser>");
    }
    if chart.kind != ChartKind::Pie {
        out.push_str("<c:axId val=\"10\"/><c:axId val=\"20\"/>");
    }
    write!(out, "</c:{tag}>").unwrap();
    if chart.kind != ChartKind::Pie {
        out.push_str("<c:catAx><c:axId val=\"10\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:axPos val=\"b\"/><c:crossAx val=\"20\"/><c:crosses val=\"autoZero\"/></c:catAx><c:valAx><c:axId val=\"20\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:axPos val=\"l\"/><c:crossAx val=\"10\"/><c:crosses val=\"autoZero\"/><c:crossBetween val=\"between\"/></c:valAx>");
    }
    out.push_str("</c:plotArea><c:legend><c:legendPos val=\"r\"/></c:legend><c:plotVisOnly val=\"0\"/></c:chart></c:chartSpace>");
    Ok(out)
}

#[derive(Default)]
struct Node {
    name: String,
    attrs: Attributes,
    text: String,
    children: Vec<Node>,
}
impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|n| n.name == name)
    }
    fn descendants<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for n in &self.children {
            if n.name == name {
                out.push(n);
            }
            n.descendants(name, out);
        }
    }
    fn all(&self, name: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        self.descendants(name, &mut out);
        out
    }
    fn attr(&self, name: &str) -> &str {
        self.attrs.get(name).map(String::as_str).unwrap_or("")
    }
    fn content(&self) -> String {
        let mut t = self.text.clone();
        for c in &self.children {
            t.push_str(&c.content());
        }
        t
    }
}
fn tree(xml: &str) -> Result<Node, ServiceError> {
    let mut reader = Reader::from_str(xml);
    let mut stack = vec![Node::default()];
    let mut count = 0;
    loop {
        match reader.read_event().map_err(invalid)? {
            Event::Start(t) | Event::Empty(t) => {
                count += 1;
                if count > 500_000 || stack.len() > 64 {
                    return Err(invalid("Presentation XML tree exceeds bounds"));
                }
                let empty =
                    xml.as_bytes().get(reader.buffer_position() as usize - 2) == Some(&b'/');
                let mut node = Node {
                    name: String::from_utf8_lossy(t.local_name().as_ref()).into_owned(),
                    ..Node::default()
                };
                for attr in t.attributes() {
                    let attr = attr.map_err(invalid)?;
                    node.attrs.insert(
                        String::from_utf8_lossy(attr.key.as_ref()).into_owned(),
                        attr.decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(invalid)?
                        .into_owned(),
                    );
                }
                if empty {
                    stack.last_mut().unwrap().children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err(invalid("Unbalanced XML"));
                }
                let node = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(node);
            }
            Event::Text(t) => stack.last_mut().unwrap().text.push_str(
                &t.xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(invalid)?,
            ),
            Event::CData(t) => stack
                .last_mut()
                .unwrap()
                .text
                .push_str(&t.decode().map_err(invalid)?),
            Event::GeneralRef(t) => {
                let name = t.decode().map_err(invalid)?;
                let decoded = quick_xml::escape::unescape(&format!("&{name};"))
                    .map_err(invalid)?
                    .into_owned();
                stack.last_mut().unwrap().text.push_str(&decoded);
            }
            Event::DocType(_) => return Err(invalid("DTD declarations are not supported")),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(invalid("Unclosed XML"));
    }
    Ok(stack.pop().unwrap())
}
fn rectangle(text: &str) -> Result<Rectangle, ServiceError> {
    let normalized = text.replace('$', "");
    let (first, last) = normalized
        .split_once(':')
        .unwrap_or((&normalized, &normalized));
    let (r, c) = address(first)?;
    let (er, ec) = address(last)?;
    if er < r || ec < c || (er - r + 1).saturating_mul(ec - c + 1) > 100_000 {
        return Err(invalid("Presentation range exceeds bounds"));
    }
    Ok((r, c, er - r + 1, ec - c + 1))
}
fn grow(layout: &mut Layout, rect: Rectangle) {
    layout.rows = layout.rows.max(rect.0 + rect.2);
    layout.columns = layout.columns.max(rect.1 + rect.3);
}
fn package_relationships(
    archive: &mut zip::ZipArchive<File>,
    owner: &str,
    budget: &mut usize,
) -> Result<Vec<(String, String, String)>, ServiceError> {
    let (dir, file) = owner.rsplit_once('/').unwrap_or(("", owner));
    let rel = format!("{dir}/_rels/{file}.rels");
    let Some(xml) = part(archive, &rel, budget)? else {
        return Ok(Vec::new());
    };
    let root = tree(&xml)?;
    let mut out = Vec::new();
    for n in root.all("Relationship") {
        if n.attr("TargetMode") == "External" {
            continue;
        }
        let target = n.attr("Target");
        let full = if target.starts_with('/') {
            target_path(target)?
        } else {
            target_path(&format!("/{dir}/{target}"))?
        };
        out.push((n.attr("Id").into(), n.attr("Type").into(), full));
    }
    Ok(out)
}

pub(super) fn read(
    archive: &mut zip::ZipArchive<File>,
    sheet: (&str, &str),
    xml: &str,
    style_xml: Option<&str>,
    layout: &mut Layout,
    budget: &mut usize,
    losses: &mut BTreeSet<String>,
) -> Result<(), ServiceError> {
    let (path, name) = sheet;
    let root = tree(xml)?;
    let rels = package_relationships(archive, path, budget)?;
    for (_, kind, target) in &rels {
        if kind.ends_with("/comments") {
            let text =
                part(archive, target, budget)?.ok_or_else(|| invalid("Missing comment part"))?;
            let comments = tree(&text)?;
            for comment in comments.all("comment") {
                let (r, c) = address(comment.attr("ref"))?;
                let note = comment.child("text").map(Node::content).unwrap_or_default();
                if note.len() > 8192 || layout.notes.len() >= 10000 {
                    return Err(invalid("Notes exceed native limits"));
                }
                layout.notes.push((r, c, note));
                grow(layout, (r, c, 1, 1));
            }
            losses.insert("Comment text is preserved as native notes; author identity and rich-text formatting are omitted.".into());
        }
    }
    if let Some(filter) = root.all("autoFilter").first() {
        let columns = filter.all("filterColumn");
        let custom = filter.all("customFilter");
        let supported = columns.len() == 1
            && custom.len() == 1
            && filter.all("filters").is_empty()
            && matches!(custom[0].attr("operator"), "" | "equal");
        let needle = if supported {
            contains_pattern(custom[0].attr("val"))
        } else {
            None
        };
        if let Some(text) = needle {
            let rect = rectangle(filter.attr("ref"))?;
            let col = index(&columns[0].attrs, "colId", usize::MAX)?;
            if col >= rect.3 {
                return Err(invalid("Filter column exceeds its range"));
            }
            layout.filter = Some((rect, col, text));
            grow(layout, rect);
        } else {
            losses.insert("Unsupported AutoFilter criteria are omitted; native filters support one text-contains column.".into());
        }
    }
    let style_tree = style_xml.map(tree).transpose()?;
    let dxfs = style_tree
        .as_ref()
        .map(|r| r.all("dxf"))
        .unwrap_or_default();
    let mut rules = Vec::new();
    for cf in root.all("conditionalFormatting") {
        for rule in &cf.children {
            if rule.name != "cfRule" {
                continue;
            }
            let comparison = match rule.attr("operator") {
                "greaterThan" => Some(Comparison::Greater),
                "lessThan" => Some(Comparison::Less),
                "equal" => Some(Comparison::Equal),
                _ => None,
            };
            let value = rule
                .child("formula")
                .and_then(|n| n.content().parse::<f64>().ok())
                .filter(|n| n.is_finite());
            let style = rule
                .attr("dxfId")
                .parse::<usize>()
                .ok()
                .and_then(|id| dxfs.get(id))
                .and_then(|n| read_dxf(n).ok());
            if rule.attr("type") == "cellIs"
                && rule.attr("stopIfTrue") != "1"
                && !cf.attr("sqref").contains(' ')
            {
                if let (Some(comparison), Some(value), Some(style)) = (comparison, value, style) {
                    let rect = rectangle(cf.attr("sqref"))?;
                    grow(layout, rect);
                    rules.push((
                        index(&rule.attrs, "priority", 0)?,
                        rect,
                        comparison,
                        value,
                        style,
                    ));
                    continue;
                }
            }
            losses.insert("Unsupported conditional formatting is omitted; native rules support numeric greater/less/equal thresholds.".into());
        }
    }
    // Native later matching rules win. Excel's lower priority number wins.
    rules.sort_by_key(|r| std::cmp::Reverse(r.0));
    if rules.len() > 32 {
        return Err(invalid("Too many conditional rules"));
    }
    layout.conditional = rules
        .into_iter()
        .map(|(_, r, c, v, s)| (r, c, v, s))
        .collect();
    for drawing in root.all("drawing") {
        losses.insert(
            "Chart layout, styling and unsupported drawing features use native defaults.".into(),
        );
        let Some((_, _, target)) = rels
            .iter()
            .find(|(id, kind, _)| id == drawing.attr("r:id") && kind.ends_with("/drawing"))
        else {
            continue;
        };
        let drawing_xml =
            part(archive, target, budget)?.ok_or_else(|| invalid("Missing drawing part"))?;
        let drawing_tree = tree(&drawing_xml)?;
        let drawing_rels = package_relationships(archive, target, budget)?;
        if !drawing_tree.all("pic").is_empty() || !drawing_tree.all("sp").is_empty() {
            losses.insert("Drawing images and shapes are omitted.".into());
        }
        for chart in drawing_tree.all("chart") {
            let Some((_, _, target)) = drawing_rels
                .iter()
                .find(|(id, kind, _)| id == chart.attr("r:id") && kind.ends_with("/chart"))
            else {
                continue;
            };
            let chart_xml =
                part(archive, target, budget)?.ok_or_else(|| invalid("Missing chart part"))?;
            match read_chart(&tree(&chart_xml)?, name) {
                Ok((rect, title, kind)) => {
                    if layout.charts.len() >= 16 {
                        return Err(invalid("Too many charts"));
                    }
                    grow(layout, rect);
                    layout.charts.push((rect, title, kind));
                }
                Err(_) => {
                    losses.insert("Unsupported chart omitted; native charts require bar/line/pie with contiguous same-sheet categories and 1–8 adjacent series.".into());
                }
            }
        }
    }
    Ok(())
}
fn contains_pattern(text: &str) -> Option<String> {
    let middle = text.strip_prefix('*')?.strip_suffix('*')?;
    let mut out = String::new();
    let mut chars = middle.chars();
    while let Some(c) = chars.next() {
        match c {
            '~' => out.push(chars.next()?),
            '*' | '?' => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}
fn read_dxf(n: &Node) -> Result<CellStyle, ServiceError> {
    let mut style = CellStyle::default();
    let mut losses = BTreeSet::new();
    for child in &n.children {
        match child.name.as_str() {
            "font" => {
                for f in &child.children {
                    match f.name.as_str() {
                        "b" => style.bold = f.attr("val") != "0",
                        "i" => style.italic = f.attr("val") != "0",
                        "u" => style.underline = true,
                        "sz" => style.font_size = Some(f.attr("val").parse().map_err(invalid)?),
                        "color" => style.foreground = colour(&f.attrs, &mut losses),
                        _ => return Err(invalid("Unsupported conditional font")),
                    }
                }
            }
            "fill" => {
                let pattern = child
                    .child("patternFill")
                    .ok_or_else(|| invalid("Conditional fill"))?;
                if pattern.attr("patternType") != "solid" {
                    return Err(invalid("Conditional fill pattern"));
                }
                style.background = pattern
                    .child("fgColor")
                    .and_then(|c| colour(&c.attrs, &mut losses));
            }
            "numFmt" => style.number_format = child.attr("formatCode").into(),
            "alignment" => {
                style.alignment = match child.attr("horizontal") {
                    "" | "general" => Alignment::General,
                    "left" => Alignment::Left,
                    "right" => Alignment::Right,
                    "center" => Alignment::Center,
                    _ => return Err(invalid("Conditional alignment")),
                };
                style.wrap = child.attr("wrapText") == "1";
            }
            "border" => {
                let sides: Vec<_> = child
                    .children
                    .iter()
                    .filter(|n| !n.attr("style").is_empty())
                    .collect();
                if sides.iter().any(|n| n.attr("style") != "thin") {
                    return Err(invalid("Conditional border"));
                }
                style.border = if sides.len() == 4 {
                    Border::All
                } else if sides.len() == 1 && sides[0].name == "bottom" {
                    Border::Bottom
                } else if sides.is_empty() {
                    Border::None
                } else {
                    return Err(invalid("Conditional border"));
                };
            }
            _ => return Err(invalid("Unsupported conditional style")),
        }
    }
    if !losses.is_empty() {
        return Err(invalid("Unsupported conditional colour"));
    }
    style.validate().map_err(invalid)?;
    Ok(style)
}
fn chart_ref(n: &Node, sheet: &str) -> Result<Rectangle, ServiceError> {
    let f = n
        .all("f")
        .first()
        .ok_or_else(|| invalid("Missing chart reference"))?
        .content();
    let (owner, reference) = f
        .rsplit_once('!')
        .ok_or_else(|| invalid("Chart reference"))?;
    let owner = owner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(owner)
        .replace("''", "'");
    if owner != sheet {
        return Err(invalid("Cross-sheet chart"));
    }
    rectangle(reference)
}
fn read_chart(root: &Node, sheet: &str) -> Result<(Rectangle, String, ChartKind), ServiceError> {
    let plot = root
        .all("plotArea")
        .first()
        .copied()
        .ok_or_else(|| invalid("Chart plot"))?;
    let charts: Vec<_> = plot
        .children
        .iter()
        .filter(|n| n.name.ends_with("Chart"))
        .collect();
    if charts.len() != 1 {
        return Err(invalid("Combination chart"));
    }
    let chart = charts[0];
    let kind = match chart.name.as_str() {
        "barChart" => ChartKind::Bar,
        "lineChart" => ChartKind::Line,
        "pieChart" => ChartKind::Pie,
        _ => return Err(invalid("Chart kind")),
    };
    if chart
        .child("grouping")
        .is_some_and(|n| !matches!(n.attr("val"), "standard" | "clustered"))
        || chart
            .child("barDir")
            .is_some_and(|n| n.attr("val") != "col")
    {
        return Err(invalid("Chart layout"));
    }
    let series = chart.all("ser");
    if series.is_empty() || series.len() > 8 {
        return Err(invalid("Chart series"));
    }
    let mut bounds = None;
    for (i, ser) in series.iter().enumerate() {
        let cat = chart_ref(
            ser.child("cat")
                .ok_or_else(|| invalid("Chart categories"))?,
            sheet,
        )?;
        let val = chart_ref(
            ser.child("val").ok_or_else(|| invalid("Chart values"))?,
            sheet,
        )?;
        if cat.0 == 0
            || cat.3 != 1
            || val.3 != 1
            || cat.0 != val.0
            || cat.2 != val.2
            || val.1 != cat.1 + i + 1
        {
            return Err(invalid("Chart series layout"));
        }
        let header = chart_ref(
            ser.child("tx")
                .ok_or_else(|| invalid("Chart series heading"))?,
            sheet,
        )?;
        if header != (cat.0 - 1, val.1, 1, 1) {
            return Err(invalid("Chart header"));
        }
        let rect = (cat.0 - 1, cat.1, cat.2 + 1, series.len() + 1);
        if bounds.is_some_and(|r| r != rect) || rect.2 * rect.3 > 1000 {
            return Err(invalid("Chart range"));
        }
        bounds = Some(rect);
    }
    let title = root
        .all("title")
        .first()
        .map(|n| n.all("t").iter().map(|t| t.content()).collect::<String>())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Chart".into());
    if title.len() > 255 {
        return Err(invalid("Chart title"));
    }
    Ok((bounds.unwrap(), title, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_is_bounded_and_does_not_expand_dtd_entities() {
        assert!(
            tree("<!DOCTYPE x [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><x>&secret;</x>")
                .is_err()
        );
        assert!(tree(&format!("{}{}", "<x>".repeat(66), "</x>".repeat(66))).is_err());
        assert_eq!(
            tree("<x>A&amp;B&lt;C&gt;&#10;</x>").unwrap().all("x")[0].content(),
            "A&B<C>\n"
        );
        assert!(target_path("/../../outside.xml").is_err());
        assert!(target_path("https://example.com/chart.xml").is_err());
    }
    #[test]
    fn filter_literals_preserve_wildcard_characters() {
        assert_eq!(contains_pattern("*a~*b~?c~~d*"), Some("a*b?c~d".into()));
        assert_eq!(contains_pattern("*a?b*"), None);
        assert_eq!(contains_pattern("exact"), None);
    }
}
