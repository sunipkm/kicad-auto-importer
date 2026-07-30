//! Build the `pcbdata` JSON blob and render the interactive HTML BOM.
//!
//! `build_pcbdata` converts our parsed `PcbBoard` + priced BOM rows into the
//! JSON schema consumed by the vendored `ibom.html`/`ibom.js` viewer
//! (see `DATAFORMAT.md` for the full schema).
//!
//! `render_html` does the `///MARKER///` template substitution defined by
//! `InteractiveHtmlBom/core/ibom.py::generate_file` — all in the same order
//! as the Python reference, with PCBDATA replaced last.

use kicad_parse::pcb::{Drawing, Footprint, Pad, PadShape, PadType, PcbBoard, Side};
use serde_json::{json, Value};

use crate::bom_pricing::PricedRow;

// ── asset embedding ───────────────────────────────────────────────────────

const IBOM_HTML: &str = include_str!("../assets/interactive_bom/ibom.html");
const IBOM_CSS: &str = include_str!("../assets/interactive_bom/ibom.css");
const SPLIT_JS: &str = include_str!("../assets/interactive_bom/split.js");
const PEP_JS: &str = include_str!("../assets/interactive_bom/pep.js");
const UTIL_JS: &str = include_str!("../assets/interactive_bom/util.js");
const RENDER_JS: &str = include_str!("../assets/interactive_bom/render.js");
const TABLE_UTIL_JS: &str = include_str!("../assets/interactive_bom/table-util.js");
const IBOM_JS: &str = include_str!("../assets/interactive_bom/ibom.js");

// ── pcbdata builder ───────────────────────────────────────────────────────

/// Build the complete `pcbdata` JSON value from a parsed PCB and priced rows.
///
/// Field names and structure follow `InteractiveHtmlBom/DATAFORMAT.md`.
/// MVP omits tracks, zones, nets, font_data, and silkscreen/fabrication drawings.
pub fn build_pcbdata(board: &PcbBoard, priced_rows: &[PricedRow]) -> Value {
    // ── edges ─────────────────────────────────────────────────────────────
    let edges: Vec<Value> = board.edges.iter().map(drawing_to_json).collect();
    let bbox = &board.edges_bbox;

    // ── footprints ────────────────────────────────────────────────────────
    // Index each footprint; numeric ID = position in this array.
    let footprint_json: Vec<Value> = board.footprints.iter().map(footprint_to_json).collect();

    // Build ref → footprint-index map for BOM assembly.
    let ref_to_id: std::collections::HashMap<&str, usize> = board
        .footprints
        .iter()
        .enumerate()
        .map(|(i, fp)| (fp.reference.as_str(), i))
        .collect();

    // ── bom rows ──────────────────────────────────────────────────────────
    // bom.both = [ [[ref, fp_id], ...], ... ]  one inner array per group
    // bom.F / bom.B = same, filtered by footprint layer
    let mut both: Vec<Value> = Vec::new();
    let mut front: Vec<Value> = Vec::new();
    let mut back: Vec<Value> = Vec::new();

    // fields: { "fp_id": [value, footprint_type, price, vendor] }
    let mut fields: serde_json::Map<String, Value> = serde_json::Map::new();

    for row in priced_rows {
        let refs: Vec<(String, usize)> = row
            .group
            .references
            .iter()
            .filter_map(|r| ref_to_id.get(r.as_str()).map(|&id| (r.clone(), id)))
            .collect();

        if refs.is_empty() {
            continue;
        }

        let (price_str, vendor_str) = match &row.outcome {
            Ok(offer) => (format!("{:.4}", offer.unit_price), offer.seller.clone()),
            Err(_) => (String::new(), String::new()),
        };

        // Populate fields for each referenced footprint.
        for (_, id) in &refs {
            let fp = &board.footprints[*id];
            fields.insert(
                id.to_string(),
                json!([fp.value, fp.footprint_type, price_str, vendor_str]),
            );
        }

        let bom_row: Value = json!(refs.iter().map(|(r, id)| json!([r, id])).collect::<Vec<_>>());

        let f_refs: Vec<Value> = refs
            .iter()
            .filter(|(_, id)| board.footprints[*id].layer == Side::Front)
            .map(|(r, id)| json!([r, id]))
            .collect();
        let b_refs: Vec<Value> = refs
            .iter()
            .filter(|(_, id)| board.footprints[*id].layer == Side::Back)
            .map(|(r, id)| json!([r, id]))
            .collect();

        both.push(bom_row);
        if !f_refs.is_empty() {
            front.push(json!(f_refs));
        }
        if !b_refs.is_empty() {
            back.push(json!(b_refs));
        }
    }

    // ── metadata ──────────────────────────────────────────────────────────
    let meta = &board.metadata;

    json!({
        "ibom_version": "v3.0.0",
        "edges_bbox": {
            "minx": bbox.minx,
            "miny": bbox.miny,
            "maxx": bbox.maxx,
            "maxy": bbox.maxy,
        },
        "edges": edges,
        "drawings": {
            "silkscreen": { "F": [], "B": [] },
            "fabrication": { "F": [], "B": [] },
        },
        "footprints": footprint_json,
        "metadata": {
            "title": meta.title,
            "revision": meta.revision,
            "company": meta.company,
            "date": meta.date,
        },
        "bom": {
            "both": both,
            "F": front,
            "B": back,
            "skipped": [],
            "fields": fields,
        },
    })
}

// ── HTML renderer ─────────────────────────────────────────────────────────

/// Render the self-contained interactive HTML BOM by injecting `pcbdata`
/// and config into the vendored `ibom.html` template.
///
/// Marker replacement order follows `ibom.py::generate_file` exactly;
/// PCBDATA is replaced last for performance (largest substitution).
///
/// The embedded JSON is hardened against `</script` injection that would
/// let adversarial schematic property text break out of the `<script>` block.
///
/// `dark_mode` seeds ibom.js's dark-mode checkbox with the host OS's current
/// theme; the viewer's own checkbox can still override it afterwards.
pub fn render_html(pcbdata: &Value, dark_mode: bool) -> String {
    let config = json!({
        "dark_mode": dark_mode,
        "show_pads": true,
        "show_fabrication": false,
        "show_silkscreen": false,
        "highlight_pin1": "selected",
        "redraw_on_drag": true,
        "board_rotation": 0,
        "checkboxes": "",
        "bom_view": "left-right",
        "layer_view": "FB",
        "offset_back_rotation": false,
        "kicad_text_formatting": false,
        "mark_when_checked": "",
        "fields": ["Value", "Footprint", "Price", "Vendor"],
    });

    let config_js = format!("var config = {}", serde_json::to_string(&config).unwrap());
    let pcbdata_js = format!(
        "var pcbdata = {}",
        escape_script(serde_json::to_string(pcbdata).unwrap())
    );

    IBOM_HTML
        .replace("///CSS///", IBOM_CSS)
        .replace("///USERCSS///", "")
        .replace("///SPLITJS///", SPLIT_JS)
        .replace("///LZ-STRING///", "")
        .replace("///POINTER_EVENTS_POLYFILL///", PEP_JS)
        .replace("///CONFIG///", &config_js)
        .replace("///UTILJS///", UTIL_JS)
        .replace("///RENDERJS///", RENDER_JS)
        .replace("///TABLEUTILJS///", TABLE_UTIL_JS)
        .replace("///IBOMJS///", IBOM_JS)
        .replace("///USERJS///", "")
        .replace("///USERHEADER///", "")
        .replace("///USERFOOTER///", "")
        .replace("///PCBDATA///", &pcbdata_js)
}

/// Escape `</script` so embedded JSON can't break out of a `<script>` block.
fn escape_script(s: String) -> String {
    s.replace("</script", r"<\/script")
}

// ── drawing conversion ────────────────────────────────────────────────────

fn drawing_to_json(d: &Drawing) -> Value {
    match d {
        Drawing::Segment { start, end, width } => json!({
            "type": "segment",
            "start": start,
            "end": end,
            "width": width,
        }),
        Drawing::Rect { start, end, width } => json!({
            "type": "rect",
            "start": start,
            "end": end,
            "width": width,
        }),
        Drawing::Circle { center, radius, width, .. } => json!({
            "type": "circle",
            "start": center,
            "radius": radius,
            "filled": 0,
            "width": width,
        }),
        Drawing::Arc { center, radius, startangle, endangle, width } => json!({
            "type": "arc",
            "start": center,
            "radius": radius,
            "startangle": startangle,
            "endangle": endangle,
            "width": width,
        }),
        Drawing::Polygon { polygons, pos, angle, filled, width } => json!({
            "type": "polygon",
            "filled": if *filled { 1 } else { 0 },
            "width": width,
            "pos": pos,
            "angle": angle,
            "polygons": polygons,
        }),
    }
}

// ── footprint conversion ──────────────────────────────────────────────────

fn footprint_to_json(fp: &Footprint) -> Value {
    let pads: Vec<Value> = fp.pads.iter().map(pad_to_json_real).collect();
    json!({
        "ref": fp.reference,
        "center": fp.center,
        "bbox": {
            "pos": fp.bbox.pos,
            "angle": fp.bbox.angle,
            "relpos": fp.bbox.relpos,
            "size": fp.bbox.size,
        },
        "pads": pads,
        "drawings": [],
        "layer": fp.layer.as_str(),
    })
}

fn pad_to_json_real(pad: &Pad) -> Value {

    let layers: Vec<&str> = pad.layers.iter().map(String::as_str).collect();

    let mut obj = serde_json::Map::new();
    obj.insert("layers".into(), json!(layers));
    obj.insert("pos".into(), json!(pad.pos));
    obj.insert("size".into(), json!(pad.size));
    obj.insert("angle".into(), json!(pad.angle));
    obj.insert("shape".into(), json!(pad.shape.as_str()));
    obj.insert("type".into(), json!(pad.pad_type.as_str()));

    if pad.pin1 {
        obj.insert("pin1".into(), json!(1));
    }

    match &pad.shape {
        PadShape::Roundrect { rratio } => {
            let min_dim = pad.size[0].min(pad.size[1]);
            obj.insert("radius".into(), json!(rratio * min_dim / 2.0));
        }
        PadShape::Chamfrect { rratio, chamfpos, chamfratio } => {
            let min_dim = pad.size[0].min(pad.size[1]);
            obj.insert("radius".into(), json!(rratio * min_dim / 2.0));
            obj.insert("chamfpos".into(), json!(chamfpos));
            obj.insert("chamfratio".into(), json!(chamfratio));
        }
        _ => {}
    }

    if pad.pad_type == PadType::Th {
        if let Some(ds) = pad.drill_shape.as_deref() {
            obj.insert("drillshape".into(), json!(ds));
        }
        if let Some(sz) = &pad.drill_size {
            obj.insert("drillsize".into(), json!(sz));
        }
    }

    if let Some(net) = &pad.net {
        obj.insert("net".into(), json!(net));
    }

    Value::Object(obj)
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kicad_parse::pcb::{BBox, BoardMetadata, PcbBoard};

    fn empty_board() -> PcbBoard {
        PcbBoard {
            footprints: vec![],
            edges: vec![],
            edges_bbox: BBox {
                minx: 0.0,
                miny: 0.0,
                maxx: 100.0,
                maxy: 80.0,
            },
            metadata: BoardMetadata {
                title: "Test Board".into(),
                revision: "1".into(),
                company: "ACME".into(),
                date: "2026-07-30".into(),
            },
        }
    }

    #[test]
    fn build_pcbdata_empty_board() {
        let board = empty_board();
        let data = build_pcbdata(&board, &[]);
        assert_eq!(data["edges_bbox"]["minx"], 0.0);
        assert_eq!(data["metadata"]["title"], "Test Board");
        assert!(data["bom"]["both"].as_array().unwrap().is_empty());
        // ibom.js's populateMetadata() does
        // `/^v\d+\.\d+/.exec(pcbdata.ibom_version)[0]` unconditionally on
        // page load; a missing/malformed version string throws and aborts
        // rendering entirely, so guard the format here.
        assert!(data["ibom_version"].as_str().unwrap().starts_with('v'));
    }

    #[test]
    fn render_html_contains_config_and_pcbdata() {
        let board = empty_board();
        let data = build_pcbdata(&board, &[]);
        let html = render_html(&data, false);
        assert!(html.contains("var config ="));
        assert!(html.contains("var pcbdata ="));
        assert!(html.contains("\"layer_view\":\"FB\""));
        assert!(html.contains("\"dark_mode\":false"));
    }

    #[test]
    fn render_html_honours_dark_mode_flag() {
        let board = empty_board();
        let data = build_pcbdata(&board, &[]);
        let html = render_html(&data, true);
        assert!(html.contains("\"dark_mode\":true"));
    }

    #[test]
    fn escape_script_blocks_injection() {
        let evil = r#"{"x": "</script><script>alert(1)</script>"}"#.to_string();
        let safe = escape_script(evil);
        assert!(!safe.contains("</script>"));
        assert!(safe.contains(r"<\/script>"));
    }
}
