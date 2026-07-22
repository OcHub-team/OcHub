//! Fold-region computation for the config editors (JSON braces/brackets,
//! TOML `[section]` blocks). Line-based: a region has a visible header line
//! and a run of hidden lines when collapsed. YAML/env get no regions for now.

use crate::highlight::Lang;

/// One foldable region. Collapsing hides lines `header + 1 ..= last`
/// (inclusive of the closing bracket line); the header stays visible with a
/// `⋯` marker appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRegion {
    /// Buffer line index of the visible header (the `{` / `[` / `[section]` line).
    pub header: usize,
    /// Buffer line index of the region's last line (hidden when collapsed).
    pub last: usize,
}

impl FoldRegion {
    /// Hidden line range when this region is collapsed.
    pub fn hidden(&self) -> std::ops::RangeInclusive<usize> {
        self.header + 1..=self.last
    }
}

/// Compute all foldable regions for `content`, sorted by header line.
/// Regions may nest (JSON); each spans at least one hidden line.
pub fn fold_regions(lang: Lang, content: &str) -> Vec<FoldRegion> {
    let mut regions = match lang {
        Lang::Json => json_regions(content),
        Lang::Toml => toml_regions(content),
        Lang::Yaml | Lang::Env | Lang::Plain => Vec::new(),
    };
    regions.sort_by_key(|r| (r.header, r.last));
    regions.dedup();
    regions
}

/// Bracket-matching scan tracking strings/escapes; emits a region for every
/// `{`/`[` whose matching close sits on a later line.
fn json_regions(content: &str) -> Vec<FoldRegion> {
    let mut regions = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // open-bracket line indices
    let mut line = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for ch in content.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '\n' => line += 1,
            '{' | '[' if !in_string => stack.push(line),
            '}' | ']' if !in_string => {
                if let Some(open_line) = stack.pop() {
                    if line > open_line {
                        regions.push(FoldRegion {
                            header: open_line,
                            last: line,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    regions
}

/// Every `[section]` / `[[array]]` header folds until the last non-blank line
/// before the next header (or EOF).
fn toml_regions(content: &str) -> Vec<FoldRegion> {
    let lines: Vec<&str> = content.split('\n').collect();
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with('['))
        .map(|(i, _)| i)
        .collect();

    let mut regions = Vec::new();
    for (idx, &header) in headers.iter().enumerate() {
        let block_end = headers
            .get(idx + 1)
            .map(|&next| next.saturating_sub(1))
            .unwrap_or(lines.len().saturating_sub(1));
        // Trim trailing blank lines out of the region so collapsing a section
        // doesn't swallow the blank separator before the next header.
        let mut last = block_end;
        while last > header && lines[last].trim().is_empty() {
            last -= 1;
        }
        if last > header {
            regions.push(FoldRegion { header, last });
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_and_array_regions() {
        let src = "{\n  \"env\": {\n    \"A\": \"1\"\n  },\n  \"list\": [\n    1,\n    2\n  ]\n}";
        let regions = fold_regions(Lang::Json, src);
        // Root object, env object, list array.
        assert_eq!(regions.len(), 3);
        assert!(regions.contains(&FoldRegion { header: 0, last: 8 }));
        assert!(regions.contains(&FoldRegion { header: 1, last: 3 }));
        assert!(regions.contains(&FoldRegion { header: 4, last: 7 }));
    }

    #[test]
    fn json_braces_inside_strings_ignored() {
        let src = "{\n  \"a\": \"{not a bracket]\"\n}";
        let regions = fold_regions(Lang::Json, src);
        assert_eq!(regions, vec![FoldRegion { header: 0, last: 2 }]);
    }

    #[test]
    fn toml_sections_fold_until_next_header() {
        let src = "top = 1\n[a]\nx = 1\ny = 2\n\n[b]\nz = 3";
        let regions = fold_regions(Lang::Toml, src);
        assert_eq!(
            regions,
            vec![
                FoldRegion { header: 1, last: 3 },
                FoldRegion { header: 5, last: 6 },
            ]
        );
    }

    #[test]
    fn single_line_json_has_no_regions() {
        assert!(fold_regions(Lang::Json, r#"{"a": 1}"#).is_empty());
        assert!(fold_regions(Lang::Yaml, "a:\n  b: 1").is_empty());
    }
}
