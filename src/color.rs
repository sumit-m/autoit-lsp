//! `textDocument/documentColor` + `textDocument/colorPresentation` —
//! inline color swatches on AutoIt color literals.
//!
//! v0.6.0. AutoIt's GUI APIs take colors as `0x…` integer literals, but the
//! byte order is **not** uniform across functions:
//!
//! * Native `GUICtrl*` / `GUISetBkColor` setters document **RGB** (`0xRRGGBB`)
//!   — AutoIt swaps to the Win32 byte order internally, so the user passes RGB.
//! * Several `_GUICtrl*` UDF wrappers pass a raw Win32 `COLORREF` and document
//!   **BGR** (`0x00BBGGRR`) — "Color must be in BGR format" in their Remarks.
//!
//! Treating everything as RGB would render the BGR swatches with red and blue
//! swapped (a "mirrored" swatch). So every entry in [`COLOR_FUNCTIONS`] carries
//! the encoding verified against that function's official AutoIt doc page.
//!
//! Scope (locked in CLAUDE.md, v0.6.0 Document color):
//! * Only **literal** `0x…` arguments get a swatch. A variable or named
//!   constant (`$COLOR_RED`) can't be resolved to a value, so it's skipped.
//! * Decimal literals are skipped too — a bare integer is ambiguous (it could
//!   be a control ID, a flag, anything) and would produce false swatches.
//! * GDI+ ARGB functions and `_GUICtrlMonthCal_SetColor` are **excluded**:
//!   ARGB pulls in alpha-channel handling whose rendering is unverified in Zed,
//!   and MonthCal's doc page doesn't state its byte order.
//!
//! Zed (1.4.4) renders the swatch via `lsp_document_colors`; the click-to-edit
//! color picker isn't implemented Zed-side yet ([zed#52208]), so the
//! [`color_presentations`] round-trip is correct-but-dormant — it lights up
//! automatically when Zed ships the picker.
//!
//! [zed#52208]: https://github.com/zed-industries/zed/issues/52208

use tower_lsp::lsp_types::{Color, ColorInformation, ColorPresentation, Range};
use tree_sitter::{Node, Tree};

use crate::tree::byte_to_position;

/// Byte order of a color literal argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    /// `0xRRGGBB` — standard RGB. Native GUI setters (AutoIt swaps internally).
    Rgb,
    /// `0x00BBGGRR` — Win32 `COLORREF` byte order. `_GUICtrl*` UDFs that pass a
    /// raw `COLORREF` to a window message.
    Bgr,
}

/// `(function name, 0-based color-argument index, encoding)`.
///
/// Every entry's encoding was verified against the official AutoIt docs (the
/// parameter description / Remarks section). Case-insensitive match — AutoIt
/// identifiers are case-insensitive.
const COLOR_FUNCTIONS: &[(&str, usize, Encoding)] = &[
    // Native GUI setters — documented "The RGB color to use."
    ("GUICtrlSetColor", 1, Encoding::Rgb),
    ("GUICtrlSetBkColor", 1, Encoding::Rgb),
    ("GUICtrlSetDefColor", 0, Encoding::Rgb),
    ("GUICtrlSetDefBkColor", 0, Encoding::Rgb),
    ("GUISetBkColor", 0, Encoding::Rgb),
    // _GUICtrl* Win32 COLORREF wrappers — documented "Color must be in BGR format".
    ("_GUICtrlListView_SetTextColor", 1, Encoding::Bgr),
    ("_GUICtrlListView_SetBkColor", 1, Encoding::Bgr),
    ("_GUICtrlListView_SetTextBkColor", 1, Encoding::Bgr),
    ("_GUICtrlStatusBar_SetBkColor", 1, Encoding::Bgr),
    ("_GUICtrlRichEdit_SetCharColor", 1, Encoding::Bgr),
    ("_GUICtrlRichEdit_SetBkColor", 1, Encoding::Bgr),
];

/// Look up a function name in [`COLOR_FUNCTIONS`] (case-insensitive).
fn lookup_color_fn(name: &str) -> Option<(usize, Encoding)> {
    COLOR_FUNCTIONS
        .iter()
        .find(|(fname, _, _)| fname.eq_ignore_ascii_case(name))
        .map(|(_, idx, enc)| (*idx, *enc))
}

// ─── documentColor ──────────────────────────────────────────────────────────

/// Collect a [`ColorInformation`] for every color-function call whose color
/// argument is a literal `0x…` value.
pub fn document_colors(tree: &Tree, source: &str) -> Vec<ColorInformation> {
    let mut out = Vec::new();
    collect(tree.root_node(), source, &mut out);
    out
}

fn collect(node: Node, source: &str, out: &mut Vec<ColorInformation>) {
    if node.kind() == "call_expression"
        && let Some(info) = color_for_call(node, source)
    {
        out.push(info);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

fn color_for_call(call: Node, source: &str) -> Option<ColorInformation> {
    let (arg, encoding) = color_arg_of(call, source)?;
    let (range, color) = parse_color_literal(arg, source, encoding)?;
    Some(ColorInformation { range, color })
}

/// Resolve the color-argument node and its encoding for a call expression,
/// or `None` if the callee isn't a known color function. Does **not** check
/// that the argument is a literal — [`parse_color_literal`] does that.
fn color_arg_of<'a>(call: Node<'a>, source: &str) -> Option<(Node<'a>, Encoding)> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "identifier" {
        return None;
    }
    let name = func.utf8_text(source.as_bytes()).ok()?;
    let (arg_index, encoding) = lookup_color_fn(name)?;
    let args = call.child_by_field_name("arguments")?;
    let arg = nth_argument(args, arg_index)?;
    Some((arg, encoding))
}

/// The nth argument expression node, skipping the `(`, `)`, and `,` tokens.
fn nth_argument(args: Node, index: usize) -> Option<Node> {
    let mut cursor = args.walk();
    args.children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "(" | ")" | ","))
        .nth(index)
}

/// Parse a `number` node as a hex color literal, returning its document range
/// and decoded [`Color`]. `None` for non-`number` nodes, non-hex literals, or
/// values that don't fit a 24-bit color.
fn parse_color_literal(node: Node, source: &str, encoding: Encoding) -> Option<(Range, Color)> {
    if node.kind() != "number" {
        return None;
    }
    let text = node.utf8_text(source.as_bytes()).ok()?;
    // Only hex literals — a decimal literal is too ambiguous to swatch.
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Keep the low 24 bits (a 6-hex-digit color space); a longer literal with
    // a stray high byte is masked rather than rejected.
    let value = u32::from_str_radix(hex, 16).ok()? & 0x00FF_FFFF;
    let color = decode(value, encoding);
    let range = Range {
        start: byte_to_position(source, node.start_byte()),
        end: byte_to_position(source, node.end_byte()),
    };
    Some((range, color))
}

/// Decode a 24-bit integer into an opaque [`Color`] per `encoding`.
fn decode(value: u32, encoding: Encoding) -> Color {
    let (r, g, b) = match encoding {
        // 0xRRGGBB
        Encoding::Rgb => ((value >> 16) & 0xFF, (value >> 8) & 0xFF, value & 0xFF),
        // 0x00BBGGRR — low byte is red, high byte is blue.
        Encoding::Bgr => (value & 0xFF, (value >> 8) & 0xFF, (value >> 16) & 0xFF),
    };
    Color {
        red: r as f32 / 255.0,
        green: g as f32 / 255.0,
        blue: b as f32 / 255.0,
        alpha: 1.0,
    }
}

// ─── colorPresentation ──────────────────────────────────────────────────────

/// Format the picked color back into the literal at `range`, matching the
/// encoding of the enclosing color function. Best-effort: if the function at
/// `range` can't be resolved, falls back to RGB. (Dormant until Zed ships the
/// color picker, but kept correct so it works the moment it does.)
pub fn color_presentations(
    tree: &Tree,
    source: &str,
    color: Color,
    range: Range,
) -> Vec<ColorPresentation> {
    let encoding = encoding_at(tree.root_node(), source, &range).unwrap_or(Encoding::Rgb);
    vec![ColorPresentation {
        label: format_color(color, encoding),
        // `text_edit: None` → the label replaces `range` by default.
        text_edit: None,
        additional_text_edits: None,
    }]
}

/// Find the encoding of the color function whose color literal spans `target`.
fn encoding_at(node: Node, source: &str, target: &Range) -> Option<Encoding> {
    if node.kind() == "call_expression"
        && let Some((arg, encoding)) = color_arg_of(node, source)
    {
        let arg_range = Range {
            start: byte_to_position(source, arg.start_byte()),
            end: byte_to_position(source, arg.end_byte()),
        };
        if arg_range == *target {
            return Some(encoding);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(enc) = encoding_at(child, source, target) {
            return Some(enc);
        }
    }
    None
}

/// Render an opaque [`Color`] as a 6-digit hex literal in `encoding` byte order.
fn format_color(color: Color, encoding: Encoding) -> String {
    let r = (color.red * 255.0).round() as u32 & 0xFF;
    let g = (color.green * 255.0).round() as u32 & 0xFF;
    let b = (color.blue * 255.0).round() as u32 & 0xFF;
    match encoding {
        Encoding::Rgb => format!("0x{r:02X}{g:02X}{b:02X}"),
        // COLORREF: high→low byte order is BB GG RR.
        Encoding::Bgr => format!("0x{b:02X}{g:02X}{r:02X}"),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    fn colors(src: &str) -> Vec<ColorInformation> {
        let tree = parse(src).expect("parse");
        document_colors(&tree, src)
    }

    /// Approximate equality for the 0..1 color floats.
    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn rgb_function_decodes_red() {
        // GUICtrlSetColor's color is the 2nd arg; 0xFF0000 in RGB is pure red.
        let c = colors("GUICtrlSetColor($id, 0xFF0000)\n");
        assert_eq!(c.len(), 1);
        let col = c[0].color;
        assert!(close(col.red, 1.0) && close(col.green, 0.0) && close(col.blue, 0.0));
        assert!(close(col.alpha, 1.0));
    }

    #[test]
    fn bgr_function_swaps_red_and_blue() {
        // Same hex digits, BGR function: 0xFF0000 is *blue*, not red.
        let c = colors("_GUICtrlListView_SetTextColor($h, 0xFF0000)\n");
        assert_eq!(c.len(), 1);
        let col = c[0].color;
        assert!(
            close(col.red, 0.0) && close(col.green, 0.0) && close(col.blue, 1.0),
            "BGR 0xFF0000 should be blue, got {col:?}"
        );
    }

    #[test]
    fn rgb_and_bgr_mirror_each_other() {
        let rgb = colors("GUICtrlSetColor($id, 0x123456)\n")[0].color;
        let bgr = colors("_GUICtrlListView_SetBkColor($h, 0x123456)\n")[0].color;
        // R and B are swapped between the two encodings; G matches.
        assert!(close(rgb.red, bgr.blue));
        assert!(close(rgb.blue, bgr.red));
        assert!(close(rgb.green, bgr.green));
    }

    #[test]
    fn function_name_is_case_insensitive() {
        let c = colors("guictrlsetcolor($id, 0x00FF00)\n");
        assert_eq!(c.len(), 1);
        assert!(close(c[0].color.green, 1.0));
    }

    #[test]
    fn arg_index_zero_function() {
        // GUISetBkColor's color is the *first* argument.
        let c = colors("GUISetBkColor(0x0000FF)\n");
        assert_eq!(c.len(), 1);
        assert!(close(c[0].color.blue, 1.0));
    }

    #[test]
    fn color_at_wrong_arg_index_is_ignored() {
        // For GUICtrlSetColor the color is arg 1; a hex in arg 0 (the control
        // id slot) must NOT be swatched.
        let c = colors("GUICtrlSetColor(0xFF0000, $textColor)\n");
        assert!(c.is_empty(), "hex in the id slot should not swatch: {c:?}");
    }

    #[test]
    fn decimal_literal_is_not_swatched() {
        let c = colors("GUICtrlSetColor($id, 16711680)\n");
        assert!(c.is_empty(), "decimal literals must be skipped: {c:?}");
    }

    #[test]
    fn variable_argument_is_not_swatched() {
        let c = colors("GUICtrlSetColor($id, $COLOR_RED)\n");
        assert!(c.is_empty());
    }

    #[test]
    fn unknown_function_is_not_swatched() {
        let c = colors("SomeOtherFunc($id, 0xFF0000)\n");
        assert!(c.is_empty());
    }

    #[test]
    fn monthcal_is_excluded() {
        // Deliberately excluded — its doc page doesn't state the byte order.
        let c = colors("_GUICtrlMonthCal_SetColor($h, 1, 0xFF0000)\n");
        assert!(c.is_empty());
    }

    #[test]
    fn range_covers_exactly_the_literal() {
        let src = "GUICtrlSetColor($id, 0xFF0000)\n";
        let c = colors(src);
        assert_eq!(c.len(), 1);
        let r = c[0].range;
        // The literal starts at column 21 and is 8 chars long ("0xFF0000").
        assert_eq!(r.start.line, 0);
        assert_eq!(r.end.line, 0);
        let start = r.start.character as usize;
        let end = r.end.character as usize;
        assert_eq!(&src[start..end], "0xFF0000");
    }

    #[test]
    fn high_byte_is_masked_to_24_bits() {
        // An 8-digit literal in an RGB function keeps only the low 24 bits.
        let c = colors("GUICtrlSetColor($id, 0xAB00FF00)\n");
        assert_eq!(c.len(), 1);
        // 0x__00FF00 → green.
        assert!(close(c[0].color.green, 1.0) && close(c[0].color.red, 0.0));
    }

    #[test]
    fn multiple_calls_each_swatched() {
        let src = concat!(
            "GUICtrlSetColor($a, 0xFF0000)\n",
            "GUICtrlSetBkColor($a, 0x00FF00)\n",
            "_GUICtrlListView_SetTextColor($h, 0x0000FF)\n",
        );
        assert_eq!(colors(src).len(), 3);
    }

    // ── colorPresentation ─────────────────────────────────────────────────────

    fn presentation(src: &str) -> String {
        let tree = parse(src).expect("parse");
        let infos = document_colors(&tree, src);
        let info = &infos[0];
        let p = color_presentations(&tree, src, info.color, info.range);
        p[0].label.clone()
    }

    #[test]
    fn presentation_round_trips_rgb() {
        // Decode then re-encode an RGB literal — should reproduce it exactly.
        assert_eq!(presentation("GUICtrlSetColor($id, 0x123456)\n"), "0x123456");
    }

    #[test]
    fn presentation_round_trips_bgr() {
        // A BGR literal must round-trip in BGR byte order, not flip to RGB.
        assert_eq!(
            presentation("_GUICtrlListView_SetTextColor($h, 0x123456)\n"),
            "0x123456"
        );
    }

    #[test]
    fn presentation_falls_back_to_rgb_when_unresolved() {
        // A range that matches no color call → RGB formatting fallback.
        let tree = parse("Local $x = 1\n").unwrap();
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        let range = Range::default();
        let p = color_presentations(&tree, "Local $x = 1\n", color, range);
        assert_eq!(p[0].label, "0xFF0000");
    }
}
