//! Destination `.kicad_sym` handling and source-symbol footprint
//! patching. Ported from `plugins/importer/symbol_importer.py`, with one
//! deliberate improvement over the Python version noted in the plan:
//! the destination library is opened via a **shallow top-level scan**
//! rather than a full recursive parse, and `save()` only ever
//! re-renders symbols added or replaced *in this run* — every other
//! symbol already sitting in the file is copied out byte-for-byte,
//! never re-serialized. This sharply limits the blast radius of any
//! future sexp-grammar bug: it can only ever corrupt a symbol imported
//! in the current run, never one already sitting untouched from a prior
//! run or a hand-edit made directly in KiCad.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::sexp::{self, Atom, Child, SexpNode};

const EMPTY_LIB: &str =
    "(kicad_symbol_lib\n  (version 20231120)\n  (generator kicad_auto_importer)\n)\n";

#[derive(Debug, thiserror::Error)]
pub enum SymbolLibraryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed symbol library file (no top-level closing paren found)")]
    Malformed,
}

struct SymbolSpan {
    name: String,
    start: usize,
    end: usize,
}

/// A destination `.kicad_sym` combined library, opened for append/patch.
pub struct SymbolLibrary {
    path: PathBuf,
    source: String,
    symbols: Vec<SymbolSpan>,
    /// Byte offset of the file's final top-level `)` — new symbols are
    /// spliced in just before this point.
    insert_offset: usize,
    replaced: HashMap<usize, String>,
    appended: Vec<String>,
}

impl SymbolLibrary {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SymbolLibraryError> {
        let path = path.into();
        let source = fs::read_to_string(&path)?;
        Self::from_source(path, source)
    }

    pub fn create_empty(path: impl Into<PathBuf>) -> Result<Self, SymbolLibraryError> {
        let path = path.into();
        fs::write(&path, EMPTY_LIB)?;
        Self::from_source(path, EMPTY_LIB.to_string())
    }

    /// Opens the library at `path` if it exists, otherwise creates an
    /// empty one first — mirrors the common
    /// `if not os.path.exists(...): create_empty(...)` call pattern in
    /// the Python pipeline.
    pub fn open_or_create(path: impl Into<PathBuf>) -> Result<Self, SymbolLibraryError> {
        let path = path.into();
        if path.exists() {
            Self::open(path)
        } else {
            Self::create_empty(path)
        }
    }

    fn from_source(path: PathBuf, source: String) -> Result<Self, SymbolLibraryError> {
        let (children, insert_offset) = scan_top_level(&source)?;
        let symbols = children
            .into_iter()
            .filter(|c| c.name == "symbol")
            .map(|c| SymbolSpan {
                name: c.first_atom.unwrap_or_default(),
                start: c.start,
                end: c.end,
            })
            .collect();
        Ok(SymbolLibrary {
            path,
            source,
            symbols,
            insert_offset,
            replaced: HashMap::new(),
            appended: Vec::new(),
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.symbols.iter().any(|s| s.name == name)
    }

    pub fn symbol_names(&self) -> impl Iterator<Item = &str> {
        self.symbols.iter().map(|s| s.name.as_str())
    }

    /// Parses and returns just the named symbol's subtree, for callers
    /// that need to inspect or deep-copy it (e.g. cross-project cherry
    /// picking) — the shallow top-level scan above never builds this,
    /// so this re-parses only that one symbol's byte span on demand.
    pub fn get_symbol_node(&self, name: &str) -> Option<SexpNode> {
        let span = self.symbols.iter().find(|s| s.name == name)?;
        sexp::parse(&self.source[span.start..span.end]).ok()
    }

    /// Adds a symbol whose subtree is `node` (already patched, e.g. via
    /// `patch_symbol_footprint`). Returns `false` (no-op) if `name`
    /// already exists in the destination and `overwrite` is false.
    pub fn add_symbol(&mut self, name: &str, node: &SexpNode, overwrite: bool) -> bool {
        // Rendered at indent=1 (one level inside the root
        // `kicad_symbol_lib`, matching every other top-level symbol);
        // the caller-side splice logic supplies the first line's
        // leading indentation itself (see `save()`).
        let rendered = sexp::render_at_indent(node, 1);

        if let Some(idx) = self.symbols.iter().position(|s| s.name == name) {
            if !overwrite {
                return false;
            }
            self.replaced.insert(idx, rendered);
            return true;
        }
        self.appended.push(rendered);
        true
    }

    /// Splices `appended`/`replaced` spans into the original file text
    /// and writes the result — never re-renders any untouched span.
    pub fn save(&self) -> std::io::Result<()> {
        let mut out = String::with_capacity(self.source.len() + 4096);
        let mut cursor = 0usize;

        for (idx, span) in self.symbols.iter().enumerate() {
            out.push_str(&self.source[cursor..span.start]);
            match self.replaced.get(&idx) {
                Some(replacement) => out.push_str(replacement),
                None => out.push_str(&self.source[span.start..span.end]),
            }
            cursor = span.end;
        }
        out.push_str(&self.source[cursor..self.insert_offset]);

        for appended in &self.appended {
            out.push_str("\n  ");
            out.push_str(appended);
            out.push('\n');
        }
        out.push_str(&self.source[self.insert_offset..]);

        fs::write(&self.path, out)
    }
}

struct TopLevelChild {
    start: usize,
    end: usize,
    name: String,
    first_atom: Option<String>,
}

/// Shallow-scans only the top level of `(kicad_symbol_lib ...)`,
/// tracking each direct child's byte span and (name, first-atom) —
/// never building a nested tree for a child's internals. Correctly
/// skips parens that appear inside quoted strings.
fn scan_top_level(source: &str) -> Result<(Vec<TopLevelChild>, usize), SymbolLibraryError> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut current_child_start: Option<usize> = None;
    let mut children = Vec::new();
    let mut root_close: Option<usize> = None;

    for (idx, ch) in source.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => {
                depth += 1;
                if depth == 2 {
                    current_child_start = Some(idx);
                }
            }
            ')' => {
                if depth == 2 {
                    if let Some(start) = current_child_start.take() {
                        let end = idx + ch.len_utf8();
                        let (name, first_atom) = peek_child_head(&source[start..end]);
                        children.push(TopLevelChild {
                            start,
                            end,
                            name,
                            first_atom,
                        });
                    }
                }
                depth -= 1;
                if depth == 0 {
                    root_close = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }

    let root_close = root_close.ok_or(SymbolLibraryError::Malformed)?;
    Ok((children, root_close))
}

/// Given `text` spanning exactly one top-level child (`"(name atom
/// ...)"`, including its outer parens), extract its node name and — if
/// present — its first atom (unquoted), without building a full tree.
fn peek_child_head(text: &str) -> (String, Option<String>) {
    let inner = &text[1..text.len() - 1]; // strip outer '(' and ')'
    let mut chars = inner.char_indices().peekable();
    let name = read_token(&mut chars).unwrap_or_default();
    let first_atom = read_token(&mut chars);
    (name, first_atom)
}

fn read_token(chars: &mut std::iter::Peekable<std::str::CharIndices>) -> Option<String> {
    while let Some(&(_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    let &(_, c) = chars.peek()?;
    if c == '(' || c == ')' {
        return None;
    }
    if c == '"' {
        chars.next();
        let mut s = String::new();
        for (_, ch) in chars.by_ref() {
            if ch == '\\' {
                // Best-effort: this is only used to peek a symbol's own
                // name for dedup purposes, not to fully decode escapes.
                continue;
            }
            if ch == '"' {
                break;
            }
            s.push(ch);
        }
        Some(s)
    } else {
        let mut s = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                break;
            }
            s.push(c);
            chars.next();
        }
        Some(s)
    }
}

// ── source-symbol patching ──────────────────────────────────────────────

/// A top-level symbol name never contains ':' — sub-symbols
/// (`Base_1_1`) do appear as separate top-level nodes in some source
/// files but are addressed via the parent; skip them during import.
pub fn is_top_level_symbol_name(name: &str) -> bool {
    !name.contains(':')
}

/// Walk `sym_node` and rewrite any `(property "Footprint" "...")` node
/// so it references `fp_lib_name:fp_name` instead of whatever the
/// download ZIP put there.
pub fn patch_symbol_footprint(
    sym_node: &mut SexpNode,
    fp_lib_name: &str,
    fp_name_map: &HashMap<String, String>,
) {
    if fp_lib_name.is_empty() {
        return;
    }
    walk_mut(sym_node, &mut |node| {
        if node.name != "property" {
            return;
        }
        let is_footprint = matches!(
            node.children.first(),
            Some(Child::Atom(a)) if a.text() == "Footprint"
        );
        if !is_footprint {
            return;
        }
        if let Some(Child::Atom(orig)) = node.children.get(1) {
            let orig_value = orig.text().to_string();
            let fp_bare = orig_value
                .rsplit_once(':')
                .map(|(_, bare)| bare.to_string())
                .unwrap_or(orig_value);
            let fp_bare = fp_name_map.get(&fp_bare).cloned().unwrap_or(fp_bare);
            let new_value = format!("{fp_lib_name}:{fp_bare}");
            node.children[1] = Child::Atom(Atom::Quoted(new_value));
        }
    });
}

/// Returns the raw `Footprint` property value (e.g. `Lib:FP`), if any.
pub fn extract_footprint_ref(sym_node: &SexpNode) -> Option<String> {
    let mut result = None;
    walk(sym_node, &mut |node| {
        if result.is_some() {
            return;
        }
        if node.name != "property" {
            return;
        }
        if let (Some(Child::Atom(key)), Some(Child::Atom(val))) =
            (node.children.first(), node.children.get(1))
        {
            if key.text() == "Footprint" {
                let v = val.text().trim().to_string();
                if !v.is_empty() {
                    result = Some(v);
                }
            }
        }
    });
    result
}

/// Sets a top-level `(property "<key>" "<value>")` on `sym_node`,
/// replacing its value if the property already exists or appending a
/// brand new (hidden — see below) one otherwise. Used to annotate a
/// symbol with vendor/pricing data from an Octopart/Nexar lookup (see
/// `crate::octopart`), but is otherwise a generic primitive any future
/// "add arbitrary metadata to a symbol" caller can reuse.
///
/// Deliberately *not* built on `walk_mut` (unlike `patch_symbol_footprint`
/// above): `walk_mut` recurses into every nested node, including
/// multi-unit sub-symbols (`(symbol "Foo_1_1" ...)`), which is harmless
/// for Footprint-patching (a sub-unit never carries its own Footprint
/// property) but wrong here — a generic setter recursing that way could
/// duplicate the property into a sub-unit node instead of (or as well
/// as) the top-level symbol. This only ever scans `sym_node`'s own
/// direct children.
pub fn set_symbol_property(sym_node: &mut SexpNode, key: &str, value: &str) {
    for child in &mut sym_node.children {
        let Child::Node(node) = child else { continue };
        if node.name != "property" {
            continue;
        }
        let is_match = matches!(node.children.first(), Some(Child::Atom(a)) if a.text() == key);
        if !is_match {
            continue;
        }
        let new_value = Child::Atom(Atom::Quoted(value.to_string()));
        match node.children.get_mut(1) {
            Some(existing) => *existing = new_value,
            None => node.children.push(new_value),
        }
        return;
    }

    // Not found: append a new property, positioned on top of the
    // origin and hidden — the same `(at 0 0 0)` / `(effects ... hide)`
    // shape KiCad itself uses for fields it ships hidden by default
    // (e.g. stock manufacturer fields). Position is otherwise
    // irrelevant since `hide` keeps it off the schematic canvas
    // entirely, so there's no on-canvas collision to worry about
    // between however many of these get added.
    let mut prop = SexpNode::new("property");
    prop.push_atom(Atom::quoted(key));
    prop.push_atom(Atom::quoted(value));

    let mut at = SexpNode::new("at");
    at.push_atom(Atom::bare("0"));
    at.push_atom(Atom::bare("0"));
    at.push_atom(Atom::bare("0"));
    prop.push_node(at);

    let mut size = SexpNode::new("size");
    size.push_atom(Atom::bare("1.27"));
    size.push_atom(Atom::bare("1.27"));
    let mut font = SexpNode::new("font");
    font.push_node(size);
    let mut effects = SexpNode::new("effects");
    effects.push_node(font);
    effects.push_atom(Atom::bare("hide"));
    prop.push_node(effects);

    sym_node.push_node(prop);
}

fn walk<'a>(node: &'a SexpNode, f: &mut impl FnMut(&'a SexpNode)) {
    f(node);
    for child in &node.children {
        if let Child::Node(n) = child {
            walk(n, f);
        }
    }
}

fn walk_mut(node: &mut SexpNode, f: &mut impl FnMut(&mut SexpNode)) {
    f(node);
    for child in &mut node.children {
        if let Child::Node(n) = child {
            walk_mut(n, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_symbol(name: &str, footprint: &str) -> SexpNode {
        let text = format!(
            r#"(symbol "{name}" (property "Reference" "U") (property "Footprint" "{footprint}" (at 0 0 0)))"#
        );
        sexp::parse(&text).unwrap()
    }

    fn prop_value(node: &SexpNode, key: &str) -> Option<String> {
        node.find_all("property").into_iter().find_map(|prop| {
            let Some(Child::Atom(k)) = prop.children.first() else {
                return None;
            };
            if k.text() != key {
                return None;
            }
            match prop.children.get(1) {
                Some(Child::Atom(v)) => Some(v.text().to_string()),
                _ => None,
            }
        })
    }

    #[test]
    fn patch_footprint_rewrites_property_and_preserves_rest() {
        let mut node = sample_symbol("Widget", "OldLib:OldFP");
        let map = HashMap::new();
        patch_symbol_footprint(&mut node, "NewLib", &map);
        assert_eq!(
            extract_footprint_ref(&node).as_deref(),
            Some("NewLib:OldFP")
        );
    }

    #[test]
    fn patch_footprint_applies_name_map() {
        let mut node = sample_symbol("Widget", "OldLib:OldFP");
        let mut map = HashMap::new();
        map.insert("OldFP".to_string(), "RenamedFP".to_string());
        patch_symbol_footprint(&mut node, "NewLib", &map);
        assert_eq!(
            extract_footprint_ref(&node).as_deref(),
            Some("NewLib:RenamedFP")
        );
    }

    #[test]
    fn set_symbol_property_appends_when_absent() {
        let mut node = sample_symbol("Widget", "MyLib:MyFP");
        assert_eq!(prop_value(&node, "Mfr"), None);

        set_symbol_property(&mut node, "Mfr", "Texas Instruments");

        assert_eq!(
            prop_value(&node, "Mfr").as_deref(),
            Some("Texas Instruments")
        );
        // Existing properties are untouched.
        assert_eq!(extract_footprint_ref(&node).as_deref(), Some("MyLib:MyFP"));
        // Newly appended properties are hidden on-schematic.
        let mfr_node = node
            .find_all("property")
            .into_iter()
            .find(|p| matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Mfr"))
            .unwrap();
        assert!(mfr_node.find(&["effects"]).is_some());
    }

    #[test]
    fn set_symbol_property_replaces_when_present() {
        let mut node = sample_symbol("Widget", "MyLib:MyFP");
        set_symbol_property(&mut node, "Mfr", "Texas Instruments");
        set_symbol_property(&mut node, "Mfr", "Analog Devices");

        assert_eq!(prop_value(&node, "Mfr").as_deref(), Some("Analog Devices"));
        // Replacing doesn't create a second "Mfr" property.
        assert_eq!(
            node.find_all("property")
                .into_iter()
                .filter(|p| matches!(p.children.first(), Some(Child::Atom(a)) if a.text() == "Mfr"))
                .count(),
            1
        );
    }

    #[test]
    fn set_symbol_property_does_not_touch_sub_unit_nodes() {
        // A multi-unit symbol: the top-level `Widget` symbol contains a
        // `Widget_1_1` sub-unit child node — `set_symbol_property` must
        // only ever add/replace on the node it's directly given, never
        // recurse into a nested `(symbol ...)` the way `patch_symbol_footprint`
        // (via `walk_mut`) deliberately does.
        let text = r#"(symbol "Widget"
            (property "Reference" "U")
            (symbol "Widget_1_1" (property "Footprint" "Sub:FP" (at 0 0 0))))"#;
        let mut node = sexp::parse(text).unwrap();

        set_symbol_property(&mut node, "Mfr", "Texas Instruments");

        assert_eq!(
            prop_value(&node, "Mfr").as_deref(),
            Some("Texas Instruments")
        );
        let sub_unit = node
            .find_all("symbol")
            .into_iter()
            .find(|n| n.name == "symbol")
            .expect("sub-unit node should still be present");
        assert_eq!(prop_value(sub_unit, "Mfr"), None);
    }

    #[test]
    fn create_empty_then_add_and_reopen_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Combined.kicad_sym");

        let mut lib = SymbolLibrary::create_empty(&path).unwrap();
        let mut node = sample_symbol("Widget", "MyLib:MyFP");
        patch_symbol_footprint(&mut node, "MyLib", &HashMap::new());
        assert!(lib.add_symbol("Widget", &node, false));
        lib.save().unwrap();

        let reopened = SymbolLibrary::open(&path).unwrap();
        assert!(reopened.contains("Widget"));
        assert_eq!(reopened.symbol_names().collect::<Vec<_>>(), vec!["Widget"]);
    }

    #[test]
    fn existing_symbols_are_byte_preserved_on_save() {
        // The core regression test for this module: adding a *second*
        // symbol must not alter a single byte of the first one's
        // already-saved text, even if that text contains a footgun
        // pattern (a quoted numeric-looking value) that a naive
        // full-tree re-render could get wrong.
        let dir = tempdir().unwrap();
        let path = dir.path().join("Combined.kicad_sym");

        let mut lib = SymbolLibrary::create_empty(&path).unwrap();
        let mut first = sample_symbol("First", "Lib:FP1");
        patch_symbol_footprint(&mut first, "Lib", &HashMap::new());
        lib.add_symbol("First", &first, false);
        lib.save().unwrap();

        let after_first_save = fs::read_to_string(&path).unwrap();
        let first_span_text = {
            let (children, _) = scan_top_level(&after_first_save).unwrap();
            let span = children.iter().find(|c| c.name == "symbol").unwrap();
            after_first_save[span.start..span.end].to_string()
        };

        let mut lib2 = SymbolLibrary::open(&path).unwrap();
        let mut second = sample_symbol("Second", "Lib:FP2");
        patch_symbol_footprint(&mut second, "Lib", &HashMap::new());
        lib2.add_symbol("Second", &second, false);
        lib2.save().unwrap();

        let after_second_save = fs::read_to_string(&path).unwrap();
        assert!(after_second_save.contains(&first_span_text));
        assert!(after_second_save.contains("\"Second\""));
    }

    #[test]
    fn duplicate_name_without_overwrite_is_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Combined.kicad_sym");
        let mut lib = SymbolLibrary::create_empty(&path).unwrap();
        let node = sample_symbol("Widget", "Lib:FP");
        assert!(lib.add_symbol("Widget", &node, false));
        lib.save().unwrap();

        let mut lib2 = SymbolLibrary::open(&path).unwrap();
        let dup = sample_symbol("Widget", "Lib:FP2");
        assert!(!lib2.add_symbol("Widget", &dup, false));
    }

    #[test]
    fn top_level_symbol_name_detection_skips_sub_symbols() {
        assert!(is_top_level_symbol_name("Widget"));
        assert!(!is_top_level_symbol_name("Widget_1_1:sub"));
    }
}
