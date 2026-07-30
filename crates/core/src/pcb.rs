//! KiCad PCB (`.kicad_pcb`) parser for Interactive BOM generation.
//!
//! **Supports KiCad 8+ only** (sexp file version ≥ 20211014).
//! KiCad 8 replaced the old decidegree arc format and `(title_block ...)`
//! with real-degree angles, start/mid/end three-point arcs, and top-level
//! `(property ...)` nodes for board variables.
//!
//! What IS parsed (MVP scope):
//! * Board outline (`Edge.Cuts`) → `PcbBoard::edges` + `edges_bbox`
//! * Footprint position / angle / layer, pad positions / sizes / shapes
//! * Board metadata from top-level `(property ...)` nodes
//!
//! What is NOT parsed (kept simple for MVP):
//! * Copper tracks / zones / nets
//! * Silkscreen / fabrication layer drawings

use std::fs;
use std::path::{Path, PathBuf};

use crate::sexp::{Child, SexpError, SexpNode};

// ── public data types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct BBox {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

impl BBox {
    fn empty() -> Self {
        BBox {
            minx: f64::MAX,
            miny: f64::MAX,
            maxx: f64::MIN,
            maxy: f64::MIN,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.minx <= self.maxx && self.miny <= self.maxy
    }

    fn expand(&mut self, x: f64, y: f64) {
        if x < self.minx { self.minx = x; }
        if y < self.miny { self.miny = y; }
        if x > self.maxx { self.maxx = x; }
        if y > self.maxy { self.maxy = y; }
    }
}

impl Default for BBox {
    fn default() -> Self {
        BBox { minx: 0.0, miny: 0.0, maxx: 100.0, maxy: 100.0 }
    }
}

/// Board-level metadata (sourced from `(property ...)` nodes in KiCad 8+).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoardMetadata {
    pub title: String,
    pub revision: String,
    pub company: String,
    pub date: String,
}

/// Which face of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Front => "F",
            Side::Back => "B",
        }
    }

    fn from_layer(layer: &str) -> Option<Side> {
        if layer.starts_with("F.") || layer == "F" {
            Some(Side::Front)
        } else if layer.starts_with("B.") || layer == "B" {
            Some(Side::Back)
        } else {
            None
        }
    }
}

/// Axis-aligned bounding box in a footprint's local (pre-rotation) coordinate
/// frame, as required by the `pcbdata` JSON `bbox` field.
#[derive(Debug, Clone, PartialEq)]
pub struct FootprintBBox {
    /// Absolute position of the footprint's placement origin.
    pub pos: [f64; 2],
    /// Footprint rotation in degrees (positive = CCW in KiCad screen space).
    pub angle: f64,
    /// Top-left corner of the pad AABB relative to `pos`, in local coordinates.
    pub relpos: [f64; 2],
    /// Width and height of the pad AABB in local coordinates.
    pub size: [f64; 2],
}

#[derive(Debug, Clone, PartialEq)]
pub struct Footprint {
    pub reference: String,
    /// KiCad footprint identifier, e.g. `"Resistor_SMD:R_0402_1005Metric"`.
    pub footprint_type: String,
    /// Component value from the PCB footprint property, e.g. `"10k"`.
    pub value: String,
    pub layer: Side,
    /// Absolute centre position on the board.
    pub center: [f64; 2],
    /// Rotation in degrees (positive = CCW).
    pub angle: f64,
    pub bbox: FootprintBBox,
    pub pads: Vec<Pad>,
}

/// Pad type for the `pcbdata` `"type"` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadType {
    /// Through-hole (`thru_hole` or `np_thru_hole` in the sexp).
    Th,
    /// Surface-mount (`smd` / `connect` in the sexp).
    Smd,
}

impl PadType {
    pub fn as_str(self) -> &'static str {
        match self {
            PadType::Th => "th",
            PadType::Smd => "smd",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PadShape {
    Rect,
    Oval,
    Circle,
    Roundrect { rratio: f64 },
    Chamfrect { rratio: f64, chamfpos: u8, chamfratio: f64 },
    Custom,
}

impl PadShape {
    pub fn as_str(&self) -> &'static str {
        match self {
            PadShape::Rect => "rect",
            PadShape::Oval => "oval",
            PadShape::Circle => "circle",
            PadShape::Roundrect { .. } => "roundrect",
            PadShape::Chamfrect { .. } => "chamfrect",
            PadShape::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pad {
    pub pad_number: String,
    /// Side(s): `["F"]`, `["B"]`, or `["F", "B"]`.
    pub layers: Vec<String>,
    /// Absolute position on the board.
    pub pos: [f64; 2],
    pub size: [f64; 2],
    /// Combined rotation (footprint angle + pad-local angle), in degrees.
    pub angle: f64,
    pub shape: PadShape,
    pub pad_type: PadType,
    /// `true` if this is the first pin (per ibom's pin1 heuristic).
    pub pin1: bool,
    /// Drill shape for through-hole pads: `"circle"` or `"oblong"`.
    pub drill_shape: Option<String>,
    /// Drill diameter (circular) or [x, y] diameters (oblong).
    pub drill_size: Option<[f64; 2]>,
    pub net: Option<String>,
}

/// A single graphical drawing on the `Edge.Cuts` layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Drawing {
    Segment {
        start: [f64; 2],
        end: [f64; 2],
        width: f64,
    },
    Rect {
        start: [f64; 2],
        end: [f64; 2],
        width: f64,
    },
    Circle {
        center: [f64; 2],
        radius: f64,
        filled: bool,
        width: f64,
    },
    /// Arc described by its circumscribed-circle parameters (as required by
    /// the `pcbdata` JSON format), converted from the three-point sexp form.
    Arc {
        center: [f64; 2],
        radius: f64,
        startangle: f64,
        endangle: f64,
        width: f64,
    },
    Polygon {
        /// One or more outlines, each a list of `[x, y]` points.
        polygons: Vec<Vec<[f64; 2]>>,
        pos: [f64; 2],
        angle: f64,
        filled: bool,
        width: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PcbBoard {
    pub footprints: Vec<Footprint>,
    pub edges: Vec<Drawing>,
    pub edges_bbox: BBox,
    pub metadata: BoardMetadata,
}

#[derive(Debug, thiserror::Error)]
pub enum PcbError {
    #[error("I/O error reading '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("S-expression parse error in '{path}': {source}")]
    Sexp {
        path: PathBuf,
        #[source]
        source: SexpError,
    },
}

// ── public API ────────────────────────────────────────────────────────────

/// Find the `.kicad_pcb` file for a project directory, mirroring
/// `schematic::find_root_schematic`: prefer the file whose stem matches
/// the `.kicad_pro` in the directory, falling back to the sole `.kicad_pcb`
/// present.
pub fn find_root_pcb(project_dir: &Path) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();

    let pro_stem = entries
        .iter()
        .find(|p| p.extension().is_some_and(|ext| ext == "kicad_pro"))
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().to_string());

    if let Some(stem) = pro_stem {
        let candidate = project_dir.join(format!("{stem}.kicad_pcb"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let pcb_files: Vec<&PathBuf> = entries
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_pcb"))
        .collect();
    if pcb_files.len() == 1 {
        pcb_files.first().copied().cloned()
    } else {
        None
    }
}

/// Parse a `.kicad_pcb` file (KiCad 8+) into a `PcbBoard`.
pub fn parse_pcb(path: &Path) -> Result<PcbBoard, PcbError> {
    let text = fs::read_to_string(path).map_err(|e| PcbError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let root = SexpNode::parse(&text).map_err(|e| PcbError::Sexp {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(PcbBoard::from_sexp(&root))
}

// ── BoardMetadata ─────────────────────────────────────────────────────────

impl BoardMetadata {
    /// Read metadata from top-level `(property ...)` nodes (KiCad 8+).
    /// Older `(title_block ...)` is NOT supported.
    fn from_sexp(root: &SexpNode) -> Self {
        let mut m = BoardMetadata::default();
        for prop in root.find_all("property") {
            let key = prop_key(prop);
            let val = prop_val(prop);
            match key.as_str() {
                "PRJ_TITLE" | "TITLE" | "Title" => m.title = val,
                "BOARD_REV" | "Rev" | "REV" => m.revision = val,
                "COMPANY" | "Company" => m.company = val,
                "DATE" | "Date" => m.date = val,
                _ => {}
            }
        }
        m
    }
}

// ── PcbBoard ──────────────────────────────────────────────────────────────

impl PcbBoard {
    fn from_sexp(root: &SexpNode) -> Self {
        let metadata = BoardMetadata::from_sexp(root);
        let (edges, edges_bbox) = PcbBoard::edges_from_sexp(root);
        let footprints = root
            .find_all("footprint")
            .into_iter()
            .filter_map(Footprint::from_sexp)
            .collect();
        PcbBoard { footprints, edges, edges_bbox, metadata }
    }

    /// Collect all top-level graphic items on `Edge.Cuts` and their bbox.
    fn edges_from_sexp(root: &SexpNode) -> (Vec<Drawing>, BBox) {
        let mut drawings = Vec::new();
        let mut bbox = BBox::empty();

        for item in root.find_all("gr_line") {
            if node_layer(item) != "Edge.Cuts" { continue; }
            let start = node_xy(item, "start");
            let end = node_xy(item, "end");
            let width = stroke_width(item);
            bbox.expand(start[0], start[1]);
            bbox.expand(end[0], end[1]);
            drawings.push(Drawing::Segment { start, end, width });
        }
        for item in root.find_all("gr_rect") {
            if node_layer(item) != "Edge.Cuts" { continue; }
            let start = node_xy(item, "start");
            let end = node_xy(item, "end");
            let width = stroke_width(item);
            for (x, y) in [(start[0], start[1]), (start[0], end[1]),
                            (end[0], start[1]), (end[0], end[1])] {
                bbox.expand(x, y);
            }
            drawings.push(Drawing::Rect { start, end, width });
        }
        for item in root.find_all("gr_circle") {
            if node_layer(item) != "Edge.Cuts" { continue; }
            let center = node_xy(item, "center");
            let radius = dist(center, node_xy(item, "end"));
            let width = stroke_width(item);
            let filled = node_bool_flag(item, "fill", "yes");
            bbox.expand(center[0] - radius, center[1] - radius);
            bbox.expand(center[0] + radius, center[1] + radius);
            drawings.push(Drawing::Circle { center, radius, filled, width });
        }
        for item in root.find_all("gr_arc") {
            if node_layer(item) != "Edge.Cuts" { continue; }
            if let Some(d) = Drawing::arc_from_sexp(item, stroke_width(item)) {
                if let Drawing::Arc { center, radius, .. } = &d {
                    bbox.expand(center[0] - radius, center[1] - radius);
                    bbox.expand(center[0] + radius, center[1] + radius);
                }
                drawings.push(d);
            }
        }
        for item in root.find_all("gr_poly") {
            if node_layer(item) != "Edge.Cuts" { continue; }
            if let Some(d) = Drawing::poly_from_sexp(item, [0.0, 0.0], 0.0, stroke_width(item), &mut bbox) {
                drawings.push(d);
            }
        }

        let final_bbox = if bbox.is_valid() { bbox } else { BBox::default() };
        (drawings, final_bbox)
    }
}

// ── Footprint ─────────────────────────────────────────────────────────────

impl Footprint {
    fn from_sexp(fp: &SexpNode) -> Option<Self> {
    let layer_str = fp.find(&["layer"])
        .and_then(|n| n.first_atom())
        .map(|a| a.text().to_string())
        .unwrap_or_default();
    let side = Side::from_layer(&layer_str)?;

    let (pos, fp_angle) = parse_at(fp);
    let reference = fp_property(fp, "Reference");
    let value = fp_property(fp, "Value");
    let footprint_type = fp.first_atom().map(|a| a.text().to_string()).unwrap_or_default();

    // ── pin1 detection ────────────────────────────────────────────────────
    const PIN1_NAMES: &[&str] = &["1", "A", "A1", "P1", "PAD1"];
    let pad_nodes = fp.find_all("pad");
    let has_standard_pin1 = pad_nodes.iter().any(|p| {
        pad_number_str(p)
            .map(|n| PIN1_NAMES.contains(&n.as_str()))
            .unwrap_or(false)
    });
    let lex_min_name: Option<String> = if !has_standard_pin1 {
        pad_nodes.iter()
            .filter_map(|p| pad_number_str(p))
            .min()
    } else {
        None
    };

    // ── parse pads + compute local AABB ───────────────────────────────────
    let mut local_minx = f64::MAX;
    let mut local_miny = f64::MAX;
    let mut local_maxx = f64::MIN;
    let mut local_maxy = f64::MIN;

    let pads: Vec<Pad> = pad_nodes
        .iter()
        .filter_map(|p| {
            let num = pad_number_str(p).unwrap_or_default();
            let pin1 = if has_standard_pin1 {
                PIN1_NAMES.contains(&num.as_str())
            } else {
                lex_min_name.as_deref() == Some(&num)
            };

            // Accumulate local AABB (pad `(at ...)` is in footprint-local space)
            let (local_pt, _) = parse_at(p);
            let size_node = p.find(&["size"]);
            let pw = size_node.and_then(|n| n.children.first()).and_then(atom_f64).unwrap_or(1.0);
            let ph = size_node.and_then(|n| n.children.get(1)).and_then(atom_f64).unwrap_or(1.0);
            if local_pt[0] - pw / 2.0 < local_minx { local_minx = local_pt[0] - pw / 2.0; }
            if local_pt[1] - ph / 2.0 < local_miny { local_miny = local_pt[1] - ph / 2.0; }
            if local_pt[0] + pw / 2.0 > local_maxx { local_maxx = local_pt[0] + pw / 2.0; }
            if local_pt[1] + ph / 2.0 > local_maxy { local_maxy = local_pt[1] + ph / 2.0; }

            Pad::from_sexp(p, pos, fp_angle, pin1)
        })
        .collect();

    let (relpos, bbox_size) = if local_minx <= local_maxx {
        (
            [local_minx, local_miny],
            [local_maxx - local_minx, local_maxy - local_miny],
        )
    } else {
        ([-1.0, -1.0], [2.0, 2.0]) // fallback for footprints with no pads
    };

    let bbox = FootprintBBox {
        pos,
        angle: fp_angle,
        relpos,
        size: bbox_size,
    };

        Some(Footprint { reference, footprint_type, value, layer: side, center: pos, angle: fp_angle, bbox, pads })
    }
}

// ── Pad ───────────────────────────────────────────────────────────────────

impl Pad {
    fn from_sexp(
        pad: &SexpNode,
        fp_pos: [f64; 2],
        fp_angle: f64,
        pin1: bool,
    ) -> Option<Self> {
    // Positional atoms: pad_number, type, shape (first three unkeyed atoms)
    let pad_number = pad_number_str(pad).unwrap_or_default();
    let type_str = positional_atom(pad, 1).unwrap_or("smd");
    let shape_str = positional_atom(pad, 2).unwrap_or("rect");

    let pad_type = match type_str {
        "thru_hole" | "np_thru_hole" => PadType::Th,
        _ => PadType::Smd,
    };

    let (local_pos, pad_angle) = parse_at(pad);
    // Negate fp_angle: ibom's renderer positions a footprint-local point via
    // canvas `rotate(-angle)` (see render.js's `drawFootprint`/bbox handling),
    // so absolute positions here must use the same rotation sense, not the
    // "plain" CCW-for-Y-up sense `rotate()` documents for its own contract.
    let abs_pos = translate(rotate(local_pos, -fp_angle), fp_pos);

    let size_node = pad.find(&["size"])?;
    let pw = size_node.children.first().and_then(atom_f64)?;
    let ph = size_node.children.get(1).and_then(atom_f64)?;

let layers = Pad::layers_from_sexp(pad);
        let shape = Pad::shape_from_sexp(pad, shape_str);
        let (drill_shape, drill_size) = Pad::drill_from_sexp(pad, &pad_type);
        let net = Pad::net_from_sexp(pad);

        Some(Pad {
            pad_number,
            layers,
            pos: abs_pos,
            size: [pw, ph],
            angle: fp_angle + pad_angle,
            shape,
            pad_type,
            pin1,
            drill_shape,
            drill_size,
            net,
        })
    }

    fn layers_from_sexp(pad: &SexpNode) -> Vec<String> {
    let mut sides = Vec::<String>::new();
    if let Some(ln) = pad.find(&["layers"]) {
        for c in &ln.children {
            if let Child::Atom(a) = c {
                let t = a.text();
                if (t.starts_with("F.") || t == "F") && !sides.contains(&"F".into()) {
                    sides.push("F".into());
                }
                if (t.starts_with("B.") || t == "B") && !sides.contains(&"B".into()) {
                    sides.push("B".into());
                }
                // "*.Cu" / "*" / "*.Mask" → both sides
                if t.starts_with("*.") || t == "*" {
                    if !sides.contains(&"F".into()) { sides.push("F".into()); }
                    if !sides.contains(&"B".into()) { sides.push("B".into()); }
                }
            }
        }
    }
        if sides.is_empty() { sides.push("F".into()); }
        sides
    }

    fn shape_from_sexp(pad: &SexpNode, shape_str: &str) -> PadShape {
    match shape_str {
        "oval" => PadShape::Oval,
        "circle" => PadShape::Circle,
        "roundrect" => {
            let rratio = pad.find(&["roundrect_rratio"])
                .and_then(|n| n.first_atom())
                .and_then(|a| a.text().parse::<f64>().ok())
                .unwrap_or(0.0);
            PadShape::Roundrect { rratio }
        }
        "chamfrect" => {
            let rratio = pad.find(&["roundrect_rratio"])
                .and_then(|n| n.first_atom())
                .and_then(|a| a.text().parse::<f64>().ok())
                .unwrap_or(0.0);
            let chamfratio = pad.find(&["chamfer_ratio"])
                .and_then(|n| n.first_atom())
                .and_then(|a| a.text().parse::<f64>().ok())
                .unwrap_or(0.0);
            let chamfpos = pad.find(&["chamfer"])
                .map(|n| {
                    let mut bits = 0u8;
                    for c in &n.children {
                        if let Child::Atom(a) = c {
                            bits |= match a.text() {
                                "top_left" => 1,
                                "top_right" => 2,
                                "bottom_left" => 4,
                                "bottom_right" => 8,
                                _ => 0,
                            };
                        }
                    }
                    bits
                })
                .unwrap_or(0);
            PadShape::Chamfrect { rratio, chamfpos, chamfratio }
        }
            // MVP has no true custom-polygon primitive parsing (ibom's
            // renderer needs a `polygons`/`svgpath` field we don't emit) —
            // fall back to the pad's anchor bounding box instead.
            "custom" => PadShape::Rect,
            _ => PadShape::Rect,
        }
    }

    fn drill_from_sexp(pad: &SexpNode, pad_type: &PadType) -> (Option<String>, Option<[f64; 2]>) {
    if !matches!(pad_type, PadType::Th) {
        return (None, None);
    }
    let Some(dn) = pad.find(&["drill"]) else {
        return (Some("circle".into()), None);
    };

    // `(drill [oval] diameter [diameter_y] [(offset x y)])`
    let first = dn.children.first().and_then(|c| if let Child::Atom(a) = c { Some(a.text()) } else { None });
    if first == Some("oval") {
        let dx = dn.children.get(1).and_then(atom_f64).unwrap_or(1.0);
        let dy = dn.children.get(2).and_then(atom_f64).unwrap_or(dx);
        (Some("oblong".into()), Some([dx, dy]))
    } else {
        let d = dn.first_atom().and_then(|a| a.text().parse::<f64>().ok()).unwrap_or(1.0);
        // Check for a second numeric atom (would make it oblong)
        let d2 = dn.children.get(1).and_then(atom_f64);
            match d2 {
                Some(dy) => (Some("oblong".into()), Some([d, dy])),
                None => (Some("circle".into()), Some([d, d])),
            }
        }
    }

    fn net_from_sexp(pad: &SexpNode) -> Option<String> {
    let net = pad.find(&["net"])?;
    // KiCad 8+: `(net "name")` — single atom.
    // KiCad 7-: `(net id "name")` — two atoms; name is second.
    let atoms: Vec<&str> = net.children.iter()
        .filter_map(|c| if let Child::Atom(a) = c { Some(a.text()) } else { None })
        .collect();
        match atoms.as_slice() {
            [name] => Some((*name).to_string()),
            [_, name] => Some((*name).to_string()),
            _ => None,
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────

impl Drawing {
    /// Convert a KiCad 8+ three-point arc into `Drawing::Arc`.
    fn arc_from_sexp(node: &SexpNode, width: f64) -> Option<Self> {
    let start = node_xy(node, "start");
    let mid = node_xy(node, "mid");
    let end = node_xy(node, "end");

    let (center, radius) = arc_circumcircle(start, mid, end)?;
    let startangle = angle_deg(start, center);
    let endangle = angle_deg(end, center);

        Some(Drawing::Arc { center, radius, startangle, endangle, width })
    }

    /// Parse an `(pts (xy x y) ...)` polygon node into `Drawing::Polygon`.
    fn poly_from_sexp(
        node: &SexpNode,
        parent_pos: [f64; 2],
        parent_angle: f64,
        width: f64,
        bbox: &mut BBox,
    ) -> Option<Self> {
    let pts = node.find(&["pts"])?;
    let points: Vec<[f64; 2]> = pts
        .find_all("xy")
        .iter()
        .map(|xy| {
            let x = xy.children.first().and_then(atom_f64).unwrap_or(0.0);
            let y = xy.children.get(1).and_then(atom_f64).unwrap_or(0.0);
            [x, y]
        })
        .collect();
    if points.is_empty() { return None; }

    for &pt in &points {
        let abs = translate(rotate(pt, parent_angle), parent_pos);
        bbox.expand(abs[0], abs[1]);
    }

    let filled = node.find(&["fill"])
        .and_then(|n| n.first_atom())
        .map(|a| a.text() != "no" && a.text() != "0")
        .unwrap_or(true);

        Some(Drawing::Polygon {
            polygons: vec![points],
            pos: parent_pos,
            angle: parent_angle,
            filled,
            width,
        })
    }
}

// ── geometry math ─────────────────────────────────────────────────────────

/// Circumscribed-circle centre and radius from three points on an arc.
/// Returns `None` when the points are collinear (degenerate arc).
pub fn arc_circumcircle(p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> Option<([f64; 2], f64)> {
    let (x1, y1) = (p1[0], p1[1]);
    let (x2, y2) = (p2[0], p2[1]);
    let (x3, y3) = (p3[0], p3[1]);

    // Perpendicular bisector of P1→P2: passes through ((x1+x2)/2, (y1+y2)/2)
    // with direction (-(y2-y1), x2-x1).
    let mx1 = (x1 + x2) / 2.0;
    let my1 = (y1 + y2) / 2.0;
    let nx1 = -(y2 - y1);
    let ny1 = x2 - x1;

    // Perpendicular bisector of P2→P3.
    let mx2 = (x2 + x3) / 2.0;
    let my2 = (y2 + y3) / 2.0;
    let nx2 = -(y3 - y2);
    let ny2 = x3 - x2;

    // Solve: (mx1 + t*nx1, my1 + t*ny1) = (mx2 + s*nx2, my2 + s*ny2)
    // Cramer's rule for t:
    let det = nx1 * (-ny2) + nx2 * ny1;
    if det.abs() < 1e-10 {
        return None; // collinear
    }
    let dx = mx2 - mx1;
    let dy = my2 - my1;
    let t = (dx * (-ny2) + nx2 * dy) / det;

    let cx = mx1 + t * nx1;
    let cy = my1 + t * ny1;
    let radius = dist([cx, cy], p1);

    Some(([cx, cy], radius))
}

/// Angle of point `p` relative to centre `c`, in degrees, measured from
/// the positive X axis going clockwise (screen space, Y pointing down) —
/// matches the ibom `pcbdata` convention.
pub fn angle_deg(p: [f64; 2], c: [f64; 2]) -> f64 {
    (p[1] - c[1]).atan2(p[0] - c[0]).to_degrees()
}

/// Rotate point `p` by `angle_deg` degrees counter-clockwise (positive =
/// CCW, matching KiCad 8+ convention) using the standard 2D rotation matrix.
pub fn rotate(p: [f64; 2], angle_deg: f64) -> [f64; 2] {
    let r = angle_deg.to_radians();
    let (cos, sin) = (r.cos(), r.sin());
    [p[0] * cos - p[1] * sin, p[0] * sin + p[1] * cos]
}

pub fn translate(p: [f64; 2], offset: [f64; 2]) -> [f64; 2] {
    [p[0] + offset[0], p[1] + offset[1]]
}

pub fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

// ── sexp convenience helpers ──────────────────────────────────────────────

/// Parse `(at x y)` or `(at x y angle)` → `([x, y], angle_deg)`.
fn parse_at(node: &SexpNode) -> ([f64; 2], f64) {
    let Some(at) = node.find(&["at"]) else {
        return ([0.0, 0.0], 0.0);
    };
    let x = at.children.first().and_then(atom_f64).unwrap_or(0.0);
    let y = at.children.get(1).and_then(atom_f64).unwrap_or(0.0);
    let a = at.children.get(2).and_then(atom_f64).unwrap_or(0.0);
    ([x, y], a)
}

/// Read `(key x y)` → `[x, y]`.
fn node_xy(node: &SexpNode, key: &str) -> [f64; 2] {
    let Some(n) = node.find(&[key]) else { return [0.0, 0.0] };
    let x = n.children.first().and_then(atom_f64).unwrap_or(0.0);
    let y = n.children.get(1).and_then(atom_f64).unwrap_or(0.0);
    [x, y]
}

/// Read `(stroke (width w) ...)` → `w`, or `(width w)` directly, or 0.
fn stroke_width(node: &SexpNode) -> f64 {
    if let Some(stroke) = node.find(&["stroke"]) {
        if let Some(w) = stroke.find(&["width"]).and_then(|n| n.first_atom()) {
            if let Ok(v) = w.text().parse::<f64>() { return v; }
        }
    }
    if let Some(w) = node.find(&["width"]).and_then(|n| n.first_atom()) {
        if let Ok(v) = w.text().parse::<f64>() { return v; }
    }
    0.0
}

/// Read `(layer "...")` from a node.
fn node_layer(node: &SexpNode) -> String {
    node.find(&["layer"])
        .and_then(|n| n.first_atom())
        .map(|a| a.text().to_string())
        .unwrap_or_default()
}

/// Test whether a node has `(flag value)` where the first atom equals `expected`.
fn node_bool_flag(node: &SexpNode, flag: &str, expected: &str) -> bool {
    node.find(&[flag])
        .and_then(|n| n.first_atom())
        .map(|a| a.text() == expected)
        .unwrap_or(false)
}

/// Extract the value of a `(property "Key" "Value" ...)` child node.
fn fp_property(node: &SexpNode, key: &str) -> String {
    for prop in node.find_all("property") {
        if prop_key(prop) == key {
            return prop_val(prop);
        }
    }
    String::new()
}

/// First (key) atom of a `(property key val ...)` node.
fn prop_key(node: &SexpNode) -> String {
    node.children.first()
        .and_then(|c| if let Child::Atom(a) = c { Some(a.text().to_string()) } else { None })
        .unwrap_or_default()
}

/// Second (value) atom of a `(property key val ...)` node.
fn prop_val(node: &SexpNode) -> String {
    node.children.get(1)
        .and_then(|c| if let Child::Atom(a) = c { Some(a.text().to_string()) } else { None })
        .unwrap_or_default()
}

/// The first unkeyed atom of a pad node — its pad number string.
fn pad_number_str(pad: &SexpNode) -> Option<String> {
    pad.children.first()
        .and_then(|c| if let Child::Atom(a) = c { Some(a.text().to_string()) } else { None })
}

/// The nth unkeyed (bare/quoted Atom, not a sub-Node) child of a node.
fn positional_atom(node: &SexpNode, n: usize) -> Option<&str> {
    node.children.iter()
        .filter_map(|c| if let Child::Atom(a) = c { Some(a.text()) } else { None })
        .nth(n)
}

/// Extract an f64 from a `Child::Atom`.
fn atom_f64(c: &Child) -> Option<f64> {
    if let Child::Atom(a) = c { a.text().parse().ok() } else { None }
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── geometry unit tests ───────────────────────────────────────────────

    #[test]
    fn circumcircle_unit_circle() {
        // P1=(1,0), P2=(0,1), P3=(-1,0) are all on the unit circle centred
        // at the origin.
        let (c, r) = arc_circumcircle([1.0, 0.0], [0.0, 1.0], [-1.0, 0.0]).unwrap();
        assert!(c[0].abs() < 1e-9, "cx={}", c[0]);
        assert!(c[1].abs() < 1e-9, "cy={}", c[1]);
        assert!((r - 1.0).abs() < 1e-9, "r={r}");
    }

    #[test]
    fn circumcircle_collinear_returns_none() {
        assert!(arc_circumcircle([0.0, 0.0], [1.0, 0.0], [2.0, 0.0]).is_none());
    }

    #[test]
    fn circumcircle_sot23_arc() {
        // Arc from the SOT-23 footprint in the test PCB (local coordinates):
        // start=(0.7,-1), mid=(1.194975,-0.794975), end=(1.4,-0.3)
        // Expected centre ≈ (0.7, -0.3), radius ≈ 0.7
        let (c, r) = arc_circumcircle(
            [0.7, -1.0],
            [1.194975, -0.794975],
            [1.4, -0.3],
        )
        .unwrap();
        assert!((c[0] - 0.7).abs() < 1e-3, "cx={}", c[0]);
        assert!((c[1] + 0.3).abs() < 1e-3, "cy={}", c[1]);
        assert!((r - 0.7).abs() < 1e-3, "r={r}");
    }

    #[test]
    fn angle_deg_cardinal_directions() {
        let o = [0.0, 0.0];
        assert!((angle_deg([1.0, 0.0], o)).abs() < 1e-9); // right → 0°
        assert!((angle_deg([0.0, 1.0], o) - 90.0).abs() < 1e-9); // down → 90°
        assert!((angle_deg([-1.0, 0.0], o).abs() - 180.0).abs() < 1e-9); // left → ±180°
        assert!((angle_deg([0.0, -1.0], o) + 90.0).abs() < 1e-9); // up → -90°
    }

    #[test]
    fn rotate_180_flips_sign() {
        let r = rotate([0.51, 0.0], 180.0);
        assert!((r[0] + 0.51).abs() < 1e-9);
        assert!(r[1].abs() < 1e-9);
    }

    #[test]
    fn rotate_90_ccw() {
        // (1,0) rotated 90° CCW (KiCad positive direction) → (0,1) in screen
        // coordinates where Y points down.
        let r = rotate([1.0, 0.0], 90.0);
        assert!(r[0].abs() < 1e-9, "x={}", r[0]);
        assert!((r[1] - 1.0).abs() < 1e-9, "y={}", r[1]);
    }

    #[test]
    fn pad_absolute_position_matches_ibom_rotation_sense() {
        // Regression test for a real bug: ibom's `render.js` positions a
        // footprint's bbox rectangle via canvas `ctx.rotate(-angle)`, so a
        // pad's absolute position must be rotated by the *negated* footprint
        // angle to land inside that same rectangle — using the raw
        // `+fp_angle` here previously placed 90°-rotated footprints' pads
        // 180° away from their (correctly-placed) bbox.
        let sexp = SexpNode::parse(
            r#"(footprint "Test:Fp"
                (layer "F.Cu")
                (at 10 20 90)
                (property "Reference" "J1" (at 0 0 0) (layer "F.SilkS"))
                (pad "1" thru_hole circle (at -2.54 0) (size 1.7 1.7)
                    (drill 1) (layers "*.Cu"))
            )"#,
        )
        .unwrap();
        let fp = Footprint::from_sexp(&sexp).expect("footprint should parse");
        let pad = &fp.pads[0];
        // Local (-2.54, 0) rotated by -90° (canvas sense) + translated by
        // (10, 20) → (10, 22.54).
        assert!((pad.pos[0] - 10.0).abs() < 1e-9, "x={}", pad.pos[0]);
        assert!((pad.pos[1] - 22.54).abs() < 1e-9, "y={}", pad.pos[1]);
    }

    // ── integration tests against the test-project PCB ───────────────────

    fn test_pcb_path() -> &'static std::path::Path {
        std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-project/kiwi-pwr-in.kicad_pcb"
        ))
    }

    #[test]
    fn find_root_pcb_finds_test_project() {
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-project"
        ));
        let found = find_root_pcb(dir);
        assert!(found.is_some(), "find_root_pcb returned None");
        assert_eq!(
            found.unwrap().extension().unwrap(),
            "kicad_pcb"
        );
    }

    #[test]
    fn parse_test_project_footprints() {
        let board = parse_pcb(test_pcb_path()).expect("parse_pcb failed");
        assert!(!board.footprints.is_empty(), "no footprints parsed");
        // Every footprint must have a non-empty reference
        for fp in &board.footprints {
            assert!(!fp.reference.is_empty(), "footprint with empty reference");
        }
    }

    #[test]
    fn parse_test_project_edges_bbox() {
        let board = parse_pcb(test_pcb_path()).expect("parse_pcb failed");
        assert!(board.edges_bbox.is_valid(), "edges_bbox is invalid");
        assert!(!board.edges.is_empty(), "no edge drawings parsed");
        // Board outline is a single gr_rect in the test project
        assert!(
            board.edges.iter().any(|d| matches!(d, Drawing::Rect { .. })),
            "expected at least one Rect edge drawing"
        );
    }

    #[test]
    fn parse_test_project_metadata() {
        let board = parse_pcb(test_pcb_path()).expect("parse_pcb failed");
        assert!(!board.metadata.title.is_empty(), "metadata title is empty");
        assert!(!board.metadata.revision.is_empty(), "metadata revision is empty");
    }

    #[test]
    fn footprint_pads_have_absolute_positions() {
        let board = parse_pcb(test_pcb_path()).expect("parse_pcb failed");
        // Verify pads have finite, board-scale positions (not the raw local
        // coordinates left un-transformed).  Components may be placed outside
        // the outline, so use a generous ±200 mm window around the bbox.
        let bbox = &board.edges_bbox;
        let margin = 200.0_f64;
        for fp in &board.footprints {
            for pad in &fp.pads {
                assert!(
                    pad.pos[0].is_finite() && pad.pos[1].is_finite(),
                    "pad in {} has non-finite position", fp.reference
                );
                assert!(
                    pad.pos[0] >= bbox.minx - margin && pad.pos[0] <= bbox.maxx + margin
                    && pad.pos[1] >= bbox.miny - margin && pad.pos[1] <= bbox.maxy + margin,
                    "pad pos [{:.3},{:.3}] in {} suspiciously far from board",
                    pad.pos[0], pad.pos[1], fp.reference
                );
            }
        }
    }
}
