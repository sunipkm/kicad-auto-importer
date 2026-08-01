//! Minimal S-expression (Lisp-style) parser and renderer for KiCad files.
//!
//! KiCad uses a subset of S-expressions:
//!   (node_name atom1 "string atom" (child_node ...) ...)
//!
//! Ported from the sibling Python plugin's `plugins/importer/sexp.py`,
//! including a bug fix discovered there: KiCad's own quoting rules
//! cannot be reconstructed from an atom's *content* (e.g. "does this
//! look like a number"). `(property "Height" "1.04" ...)` and
//! `(number "7" ...)` are ALWAYS quoted even though numeric-looking;
//! `(justify left top)` / `(type default)` and coordinate atoms are
//! ALWAYS bare even though they look similar. The only reliable rule is
//! "render an atom exactly however the source wrote it" — that's what
//! `Atom` exists to make possible.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

/// An atom's original quoting, captured at parse time (or chosen
/// deliberately when constructing a node from scratch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Atom {
    /// Was `"..."` in the source, or is fresh string data this program
    /// constructed (property values, URIs, nicknames, pin names/numbers,
    /// descriptions, paths).
    Quoted(String),
    /// Was an unquoted bareword token in the source (keywords like
    /// `left`/`yes`/`input`, or a coordinate/size number), or is fresh
    /// data this program deliberately wants rendered bare (e.g. a
    /// lib-table `version` number).
    Bare(String),
}

impl Atom {
    pub fn quoted(s: impl Into<String>) -> Self {
        Atom::Quoted(s.into())
    }

    pub fn bare(s: impl Into<String>) -> Self {
        Atom::Bare(s.into())
    }

    pub fn text(&self) -> &str {
        match self {
            Atom::Quoted(s) | Atom::Bare(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Child {
    Node(SexpNode),
    Atom(Atom),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SexpNode {
    pub name: String,
    pub children: Vec<Child>,
}

impl SexpNode {
    pub fn new(name: impl Into<String>) -> Self {
        SexpNode {
            name: name.into(),
            children: Vec::new(),
        }
    }

    /// Convenience: `(name atom)` — used when hand-building simple
    /// single-atom child nodes (e.g. a lib-table entry's fields).
    pub fn leaf(name: impl Into<String>, atom: Atom) -> Self {
        let mut node = SexpNode::new(name);
        node.children.push(Child::Atom(atom));
        node
    }

    pub fn push_node(&mut self, node: SexpNode) -> &mut Self {
        self.children.push(Child::Node(node));
        self
    }

    pub fn push_atom(&mut self, atom: Atom) -> &mut Self {
        self.children.push(Child::Atom(atom));
        self
    }

    /// First child node matching each successive path segment.
    pub fn find(&self, path: &[&str]) -> Option<&SexpNode> {
        let mut node: &SexpNode = self;
        for part in path {
            node = node.children.iter().find_map(|c| match c {
                Child::Node(n) if n.name == *part => Some(n),
                _ => None,
            })?;
        }
        Some(node)
    }

    pub fn find_all(&self, name: &str) -> Vec<&SexpNode> {
        self.children
            .iter()
            .filter_map(|c| match c {
                Child::Node(n) if n.name == name => Some(n),
                _ => None,
            })
            .collect()
    }

    /// First direct atom child, if the node has one (mirrors accessing
    /// `node.children[0]` in the Python code for a single-value node).
    pub fn first_atom(&self) -> Option<&Atom> {
        self.children.iter().find_map(|c| match c {
            Child::Atom(a) => Some(a),
            _ => None,
        })
    }

    /// Parse a complete KiCad S-expression; returns the root node.
    pub fn parse(text: &str) -> Result<Self, SexpError> {
        let tokens = tokenize(text);
        let mut pos = 0usize;
        match parse_expr(&tokens, &mut pos)? {
            Parsed::Node(n) => Ok(n),
            Parsed::Atom(_) => Err(SexpError::ExpectedNode),
        }
    }

    /// Render back to a KiCad-style S-expression string.
    pub fn render(&self) -> String {
        render_indent(self, 0)
    }

    /// Render as if nested `indent` levels deep — every line except the
    /// first carries its absolute indentation. Used when splicing a
    /// rendered subtree back into a larger file's raw text.
    pub fn render_at_indent(&self, indent: usize) -> String {
        render_indent(self, indent)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SexpError {
    #[error("unexpected end of input while parsing s-expression")]
    UnexpectedEof,
    #[error("expected a root node, got a bare atom")]
    ExpectedNode,
    #[error("unexpected ')' with no matching '('")]
    StrayCloseParen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Text(String), // raw token text, including surrounding quotes if quoted
}

fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut tokens = Vec::new();

    while i < n {
        match chars[i] {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                tokens.push(Token::Open);
                i += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                i += 1;
            }
            '"' => {
                let start = i;
                let mut j = i + 1;
                while j < n {
                    if chars[j] == '\\' {
                        j += 2;
                    } else if chars[j] == '"' {
                        j += 1;
                        break;
                    } else {
                        j += 1;
                    }
                }
                let j = j.min(n);
                tokens.push(Token::Text(chars[start..j].iter().collect()));
                i = j;
            }
            _ => {
                let start = i;
                let mut j = i;
                while j < n && !matches!(chars[j], ' ' | '\t' | '\n' | '\r' | '(' | ')' | '"') {
                    j += 1;
                }
                tokens.push(Token::Text(chars[start..j].iter().collect()));
                i = j;
            }
        }
    }
    tokens
}

fn unquote(tok: &str) -> Atom {
    if tok.len() >= 2 && tok.starts_with('"') && tok.ends_with('"') {
        let inner = &tok[1..tok.len() - 1];
        let decoded = inner
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
            .replace("\\n", "\n");
        Atom::Quoted(decoded)
    } else {
        Atom::Bare(tok.to_string())
    }
}

enum Parsed {
    Node(SexpNode),
    Atom(Atom),
}

fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<Parsed, SexpError> {
    let tok = tokens.get(*pos).ok_or(SexpError::UnexpectedEof)?;
    match tok {
        Token::Close => Err(SexpError::StrayCloseParen),
        Token::Open => {
            *pos += 1;
            let name_tok = tokens.get(*pos).ok_or(SexpError::UnexpectedEof)?;
            let name = match name_tok {
                Token::Text(s) => unquote(s).text().to_string(),
                _ => return Err(SexpError::UnexpectedEof),
            };
            *pos += 1;
            let mut node = SexpNode::new(name);
            loop {
                match tokens.get(*pos) {
                    Some(Token::Close) => {
                        *pos += 1;
                        break;
                    }
                    Some(_) => match parse_expr(tokens, pos)? {
                        Parsed::Node(n) => node.children.push(Child::Node(n)),
                        Parsed::Atom(a) => node.children.push(Child::Atom(a)),
                    },
                    None => return Err(SexpError::UnexpectedEof),
                }
            }
            Ok(Parsed::Node(node))
        }
        Token::Text(s) => {
            *pos += 1;
            Ok(Parsed::Atom(unquote(s)))
        }
    }
}

// ── renderer ─────────────────────────────────────────────────────────

fn needs_quoting(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_whitespace() || c == '(' || c == ')' || c == '"' || c == '\\')
}

fn quote_escaped(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Node *names* are quoted only if they contain characters that would
/// break tokenising — unlike atoms, a node name's bare/quoted-ness in
/// the source is never preserved (matches the Python `_quote_if_needed`,
/// which never consults `BareAtom`-ness either).
fn quote_name(s: &str) -> String {
    if needs_quoting(s) {
        quote_escaped(s)
    } else {
        s.to_string()
    }
}

fn numeric_atom_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^-?[0-9]*\.?[0-9]+$").unwrap())
}

/// `(number ...)` is exclusively the pin-number child of a symbol pin —
/// unlike `property` below, it has no other grammar meaning anywhere in
/// a KiCad file, and real KiCad always quotes it, even for purely
/// numeric pin numbers (`(number "7" ...)`). Safe to force
/// unconditionally.
fn is_always_quoted_field(node_name: &str) -> bool {
    node_name == "number"
}

/// `property` is heavily overloaded across KiCad file formats and can't
/// be force-quoted wholesale like `number` above: a footprint's
/// `(property pad_prop_castellated)` (a bareword pad-property flag, no
/// value) and `(property ki_fp_filters "...")` (bareword *name*, quoted
/// value) are both legitimate bare tokens that must stay bare. But no
/// legitimate `property` field is ever a bare, purely-numeric token —
/// real KiCad quotes even numeric-looking values (e.g. a component
/// height `"1.04"`). So this only self-heals a numeric-looking atom
/// that a previous buggy save corrupted into a bare token, without
/// touching the genuinely-bareword cases above.
fn is_self_heal_numeric_bareword_field(node_name: &str) -> bool {
    node_name == "property"
}

/// Fields whose value(s) are drawn from a small, well-known KiCad
/// grammar keyword set that must always be rendered bare (KiCad's
/// parser hard-errors on a quoted string here, e.g. `(justify "left")`).
/// Checked by (parent node name, value) so it can never misfire on an
/// unrelated field that happens to share a node name with a different
/// meaning — e.g. `(type "KiCad")` in a sym-lib-table/fp-lib-table `lib`
/// entry is left alone because "KiCad" isn't a member of this
/// stroke/fill keyword set.
fn known_bareword_fields(node_name: &str) -> Option<&'static [&'static str]> {
    match node_name {
        "justify" => Some(&["left", "right", "top", "bottom", "mirror"]),
        "type" => Some(&[
            "default",
            "solid",
            "dash",
            "dash_dot",
            "dash_dot_dot",
            "dot",
            "none",
            "outline",
            "background",
        ]),
        _ => None,
    }
}

fn quote_atom(atom: &Atom) -> String {
    match atom {
        Atom::Bare(s) => {
            if s.is_empty() || needs_quoting(s) {
                quote_escaped(s)
            } else {
                s.clone()
            }
        }
        Atom::Quoted(s) => quote_escaped(s),
    }
}

fn render_child_atom(node_name: &str, atom: &Atom) -> String {
    let force_quote = is_always_quoted_field(node_name)
        || (is_self_heal_numeric_bareword_field(node_name)
            && matches!(atom, Atom::Bare(_))
            && numeric_atom_re().is_match(atom.text()));
    if force_quote {
        return quote_escaped(atom.text());
    }
    if let Some(keywords) = known_bareword_fields(node_name) {
        if keywords.contains(&atom.text()) {
            return atom.text().to_string();
        }
    }
    quote_atom(atom)
}

fn is_inline(node: &SexpNode) -> bool {
    !node.children.iter().any(|c| matches!(c, Child::Node(_)))
}

fn render_indent(node: &SexpNode, indent: usize) -> String {
    let name_part = quote_name(&node.name);

    if node.children.is_empty() {
        return format!("({})", name_part);
    }

    if is_inline(node) {
        let atom_parts: Vec<String> = node
            .children
            .iter()
            .map(|c| match c {
                Child::Atom(a) => render_child_atom(&node.name, a),
                Child::Node(_) => unreachable!("is_inline guarantees no nested nodes"),
            })
            .collect();
        return format!("({} {})", name_part, atom_parts.join(" "));
    }

    // Mixed / nested case: walk children in order, buffering consecutive
    // atoms onto the "current line" and giving each child SexpNode its
    // own indented line.
    let mut lines: Vec<String> = Vec::new();
    let mut current: Vec<String> = vec![name_part];
    for child in &node.children {
        match child {
            Child::Node(n) => {
                lines.push(current.join(" "));
                current = Vec::new();
                let rendered_child = render_indent(n, indent + 1);
                lines.push(format!("{}{}", "  ".repeat(indent + 1), rendered_child));
            }
            Child::Atom(a) => {
                current.push(render_child_atom(&node.name, a));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current.join(" "));
    }

    format!("({})", lines.join("\n"))
}

#[allow(dead_code)]
fn known_bareword_atoms() -> HashSet<&'static str> {
    known_bareword_fields("justify")
        .unwrap_or(&[])
        .iter()
        .chain(known_bareword_fields("type").unwrap_or(&[]).iter())
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms_only(name: &str, atoms: &[Atom]) -> SexpNode {
        let mut node = SexpNode::new(name);
        for a in atoms {
            node.push_atom(a.clone());
        }
        node
    }

    // Note: these round-trip tests check that quoting/bareword-ness
    // survives a parse -> render cycle, not exact whitespace layout —
    // a node with a nested SexpNode child always renders multi-line
    // (matching real KiCad output, and this crate's own renderer, both
    // of which insert a blank line between two consecutive nested-node
    // children — see `render_indent`), so a hand-written single-line
    // fixture legitimately re-renders with different line breaks. What
    // must NOT change is whether each atom is quoted.

    #[test]
    fn roundtrips_quoted_numeric_property_value() {
        let text = r#"(property "Height" "1.04" (at 24.13 -394.92 0))"#;
        let parsed = SexpNode::parse(text).unwrap();
        let rendered = SexpNode::render(&parsed);
        assert!(
            rendered.contains(r#""Height" "1.04""#),
            "value must stay quoted even though it looks like a plain number: {rendered}"
        );
    }

    #[test]
    fn roundtrips_quoted_numeric_pin_number() {
        let text = r#"(number "7" (effects (font (size 1.27 1.27))))"#;
        let parsed = SexpNode::parse(text).unwrap();
        let rendered = SexpNode::render(&parsed);
        assert!(
            rendered.starts_with("(number \"7\""),
            "pin number must stay quoted even though it's purely numeric: {rendered}"
        );
    }

    #[test]
    fn roundtrips_bareword_justify_and_type_keywords() {
        let text = "(effects (font (size 1.27 1.27)) (justify left top))";
        let parsed = SexpNode::parse(text).unwrap();
        let rendered = SexpNode::render(&parsed);
        assert!(
            rendered.contains("(justify left top)"),
            "justify keywords must stay bare: {rendered}"
        );

        let text2 = "(stroke (width 0.254) (type default))";
        let parsed2 = SexpNode::parse(text2).unwrap();
        let rendered2 = SexpNode::render(&parsed2);
        assert!(
            rendered2.contains("(type default)"),
            "stroke type keyword must stay bare: {rendered2}"
        );
    }

    #[test]
    fn roundtrips_bareword_pad_property_flag() {
        let text = "(property pad_prop_castellated)";
        let parsed = SexpNode::parse(text).unwrap();
        assert_eq!(SexpNode::render(&parsed), text);
    }

    #[test]
    fn roundtrips_bareword_name_quoted_value_ki_fp_filters() {
        let text = r#"(property ki_fp_filters "Connector*:*_1x??-1MP*")"#;
        let parsed = SexpNode::parse(text).unwrap();
        assert_eq!(SexpNode::render(&parsed), text);
    }

    #[test]
    fn self_heals_corrupted_bare_numeric_property_value() {
        // Simulates a file already corrupted by the old buggy renderer:
        // the Height value was written bare instead of quoted. On
        // re-render it must come back quoted.
        let node = atoms_only(
            "property",
            &[Atom::Quoted("Height".into()), Atom::Bare("1.04".into())],
        );
        assert_eq!(SexpNode::render(&node), r#"(property "Height" "1.04")"#);
    }

    #[test]
    fn does_not_touch_legitimate_barewords_when_self_healing() {
        // pad_prop_castellated and ki_fp_filters must NOT be swept up by
        // the numeric self-heal rule (they aren't numeric, so the
        // numeric regex simply never matches them — this test guards
        // against a future accidental broadening of that rule).
        let flag = atoms_only("property", &[Atom::Bare("pad_prop_castellated".into())]);
        assert_eq!(SexpNode::render(&flag), "(property pad_prop_castellated)");

        let filters = atoms_only(
            "property",
            &[
                Atom::Bare("ki_fp_filters".into()),
                Atom::Quoted("R_*".into()),
            ],
        );
        assert_eq!(
            SexpNode::render(&filters),
            r#"(property ki_fp_filters "R_*")"#
        );
    }

    #[test]
    fn lib_table_version_stays_bare() {
        let mut root = SexpNode::new("sym_lib_table");
        root.push_node(SexpNode::leaf("version", Atom::bare("7")));
        assert_eq!(SexpNode::render(&root), "(sym_lib_table\n  (version 7))");
    }

    #[test]
    fn full_lib_entry_roundtrip() {
        let text = "(sym_lib_table\n  (version 7)\n\n  (lib\n    (name \"Chickadee_Stamp_v2\")\n\n    (type \"KiCad\")\n\n    (uri \"${KIPRJMOD}/chickadee-stamp-v3.pretty\")\n\n    (options \"\")\n\n    (descr \"\")))";
        let parsed = SexpNode::parse(text).unwrap();
        // Re-rendering won't reproduce the original file's blank lines
        // (those are just whitespace between tokens, discarded like any
        // other whitespace) — verify structural content survives instead.
        let entry = parsed.find_all("lib")[0];
        assert_eq!(
            entry.find(&["name"]).unwrap().first_atom().unwrap().text(),
            "Chickadee_Stamp_v2"
        );
        assert_eq!(
            entry.find(&["uri"]).unwrap().first_atom().unwrap().text(),
            "${KIPRJMOD}/chickadee-stamp-v3.pretty"
        );
    }
}
