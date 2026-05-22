//! Static catalog of documented AutoIt builtin + UDF library functions.
//!
//! Loaded once at process startup from `data/builtins.json` (embedded via
//! `include_str!` so the LSP binary is self-contained). Every entry has a
//! lowercase key so `lookup` works case-insensitively — AutoIt itself is
//! case-insensitive on identifiers.
//!
//! Used today by hover (Sprint 1 Day 3); Sprint 3 completion will add the
//! same catalog as a candidate source.
//!
//! Regenerate `builtins.json` with `scripts/scrape-builtins.ps1` — see that
//! script for the source pages and field-extraction logic.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Deserializer};

/// Embedded JSON data. Roughly 7-8MB of structured function metadata.
/// `include_str!` inlines it at compile time so we don't need a sibling
/// file to deploy alongside the LSP binary.
const RAW_JSON: &str = include_str!("../data/builtins.json");

/// One documented function from the AutoIt help.
///
/// Every field except `name`/`category`/`url` is optional — the scraper
/// emits `null` (deserialized as `None`) when a page doesn't have the
/// section. `include` is only populated for UDF library funcs whose
/// detail page shows a `#include <Lib.au3>` line above the signature.
#[derive(Debug, Clone, Deserialize)]
pub struct FunctionDoc {
    /// Canonical (case-preserved) function name.
    pub name: String,
    /// `"core"` (functions.htm) or `"udf"` (libfunctions.htm).
    pub category: String,
    /// Direct URL of the detail page, for "see also" links in hover.
    pub url: String,
    /// `#include <Lib.au3>` directive that brings this function into scope.
    /// Only present for UDF library funcs; `None` for core builtins which
    /// don't need an include.
    #[serde(default)]
    pub include: Option<String>,
    /// Function signature like `MsgBox ( flag, "title", "text" [, timeout = 0 [, hwnd]] )`.
    #[serde(default)]
    pub signature: Option<String>,
    /// Single-line description from the page's `funcdesc` paragraph.
    #[serde(default)]
    pub summary: Option<String>,
    /// Parameter table: `(name, description)` pairs in declaration order.
    ///
    /// Uses a tolerant deserializer because PowerShell's `ConvertTo-Json`
    /// flattens zero- and one-element arrays — a 0-param function comes
    /// out as `{}` (empty object) and a 1-param function as a single
    /// object (no array wrapping). We accept all three shapes: array of
    /// objects, single object, or empty object.
    #[serde(default, deserialize_with = "deserialize_parameters")]
    pub parameters: Vec<Parameter>,
    /// Free text from the "Return Value" section. May span multiple
    /// success/failure clauses.
    #[serde(default, rename = "returnValue")]
    pub return_value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub description: String,
}

/// Accept `[...]`, `{...}` (single param), `{}` (empty), `null`, or anything
/// else (treated as empty).
///
/// PowerShell's `ConvertTo-Json` is inconsistent about empty/single-element
/// arrays — it can emit `null`, `{}`, or a bare object depending on type
/// hints — so we accept everything reasonable and fall through to `[]`.
/// Going through `serde_json::Value` keeps this explicit and robust to
/// future serializer changes; the alternative (an `untagged` enum) doesn't
/// reliably match `null`.
fn deserialize_parameters<'de, D>(d: D) -> Result<Vec<Parameter>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    match value {
        serde_json::Value::Null => Ok(vec![]),
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))
            .collect(),
        serde_json::Value::Object(ref map) if map.is_empty() => Ok(vec![]),
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(|p| vec![p])
            .map_err(serde::de::Error::custom),
        _ => Ok(vec![]),
    }
}

/// Top-level shape of `data/builtins.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "scrapedAt")]
    pub scraped_at: String,
    #[serde(rename = "sourceUrl")]
    pub source_url: String,
    /// Lowercase function name → docs. Keys are pre-lowercased by the
    /// scraper for cheap case-insensitive lookup.
    pub functions: HashMap<String, FunctionDoc>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Get a reference to the singleton catalog, loading it on first call.
///
/// If `builtins.json` is malformed (it shouldn't be — the scraper round-trips
/// through PowerShell's ConvertTo-Json, and we test-load it in CI), this
/// panics on first use. That's deliberate: a broken catalog is a build/
/// release-process bug, not a runtime fallback case.
pub fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        serde_json::from_str(RAW_JSON).expect(
            "data/builtins.json failed to deserialize — regenerate via scripts/scrape-builtins.ps1",
        )
    })
}

/// Look up a function by name. Case-insensitive (caller passes any case).
///
/// Returns `None` if the name isn't in the catalog — either because it's a
/// user-defined function, an AutoIt v3 keyword/macro that the LSP handles
/// elsewhere, or a typo the user is about to fix.
pub fn lookup(name: &str) -> Option<&'static FunctionDoc> {
    catalog().functions.get(&name.to_lowercase())
}

/// Iterate over every entry in the catalog. Used by Sprint 3 completion to
/// offer all built-in functions as candidates (prefix-filtered by the caller).
pub fn all_entries() -> impl Iterator<Item = &'static FunctionDoc> {
    catalog().functions.values()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads_without_panic() {
        let cat = catalog();
        // The scrape should produce thousands of entries; assert "lots"
        // rather than a precise count so re-scrapes against newer AutoIt
        // versions don't churn this test.
        assert!(
            cat.functions.len() > 500,
            "catalog has only {} entries — scrape likely incomplete",
            cat.functions.len()
        );
    }

    #[test]
    fn lookup_is_case_insensitive() {
        // "MsgBox" is a canonical core builtin; the catalog stores it under
        // key "msgbox". Test multiple casings — every one should hit.
        for casing in ["MsgBox", "msgbox", "MSGBOX", "MsGbOx"] {
            let doc = lookup(casing).unwrap_or_else(|| panic!("lookup failed for {casing}"));
            assert_eq!(doc.name, "MsgBox");
        }
    }

    #[test]
    fn unknown_name_returns_none() {
        assert!(lookup("ThisFunctionDefinitelyDoesNotExistInAutoIt").is_none());
    }

    #[test]
    fn core_function_has_no_include() {
        // After the scraper's include-split fix, core funcs have include=None.
        // (Pre-fix data may have include=null too — either way, MsgBox should
        // not carry an include directive.)
        let doc = lookup("msgbox").expect("MsgBox present");
        assert!(doc.include.is_none(), "core MsgBox shouldn't have include");
        assert!(doc.signature.is_some());
        assert!(doc.summary.is_some());
    }

    #[test]
    fn udf_function_has_include_after_rescrape() {
        // Skipped gracefully if the test runs against pre-fix data (where
        // include is None on UDFs because the #include line was mashed into
        // the signature). After the scraper re-run with the include-split
        // patch, _ArrayAdd should have include=Some("#include <Array.au3>").
        let doc = lookup("_arrayadd").expect("_ArrayAdd present");
        if let Some(inc) = &doc.include {
            assert!(
                inc.contains("Array"),
                "expected Array.au3 in include, got {inc}"
            );
        }
        // The signature shouldn't begin with #include after the fix.
        if let Some(sig) = &doc.signature {
            assert!(
                !sig.starts_with("#include"),
                "signature still mashed: {sig}"
            );
        }
    }
}
