//! Minimal RFC 4180-style CSV reader.
//!
//! Jira and Linear issue exports are CSV, so the importers need a parser that
//! correctly handles quoted fields, escaped quotes (`""`), and commas or
//! newlines embedded inside quoted fields. This is a small, dependency-free
//! reader sufficient for those exports — not a full CSV library (it does not
//! support alternate delimiters or streaming).

/// Parse CSV `text` into rows of fields. Returns an empty vec for empty input.
///
/// Handles:
/// - quoted fields (`"a,b"` → one field `a,b`),
/// - escaped quotes inside quotes (`"he said ""hi"""` → `he said "hi"`),
/// - newlines inside quoted fields,
/// - both `\n` and `\r\n` line endings.
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    // Tracks whether the current row has seen any content, so a trailing
    // newline at EOF does not emit a spurious empty row.
    let mut row_has_content = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => {
                    in_quotes = true;
                    row_has_content = true;
                }
                ',' => {
                    row.push(std::mem::take(&mut field));
                    row_has_content = true;
                }
                '\r' => {} // swallow; the following \n ends the line
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    row_has_content = false;
                }
                _ => {
                    field.push(c);
                    row_has_content = true;
                }
            }
        }
    }

    // Flush a final unterminated row (no trailing newline).
    if row_has_content || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }

    rows
}

/// Helper over a parsed CSV: header row + lookup by column name.
pub struct Table {
    /// Lower-cased header names, in column order.
    pub headers: Vec<String>,
    /// Data rows (header row excluded).
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// Build a table from CSV text. Returns `None` when there is no header row.
    pub fn from_csv(text: &str) -> Option<Table> {
        let mut rows = parse(text);
        if rows.is_empty() {
            return None;
        }
        let headers = rows
            .remove(0)
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect();
        Some(Table { headers, rows })
    }

    /// First column index whose header equals any of `names` (case-insensitive).
    pub fn col(&self, names: &[&str]) -> Option<usize> {
        self.headers
            .iter()
            .position(|h| names.iter().any(|n| h == &n.to_ascii_lowercase()))
    }

    /// All column indices whose header equals any of `names`. Jira repeats
    /// column headers (e.g. several `Labels` columns) for multi-value fields.
    pub fn cols(&self, names: &[&str]) -> Vec<usize> {
        self.headers
            .iter()
            .enumerate()
            .filter(|(_, h)| names.iter().any(|n| *h == &n.to_ascii_lowercase()))
            .map(|(i, _)| i)
            .collect()
    }

    /// Trimmed value at `row[idx]`, or empty string when out of range.
    pub fn get<'a>(&self, row: &'a [String], idx: Option<usize>) -> &'a str {
        match idx {
            Some(i) => row.get(i).map(|s| s.trim()).unwrap_or(""),
            None => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_rows() {
        let rows = parse("a,b,c\n1,2,3\n");
        assert_eq!(rows, vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]);
    }

    #[test]
    fn handles_quotes_commas_and_newlines() {
        let rows = parse("name,note\n\"Smith, J\",\"line1\nline2\"\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1],
            vec!["Smith, J".to_string(), "line1\nline2".to_string()]
        );
    }

    #[test]
    fn handles_escaped_quotes() {
        let rows = parse("q\n\"he said \"\"hi\"\"\"\n");
        assert_eq!(rows[1], vec!["he said \"hi\"".to_string()]);
    }

    #[test]
    fn final_row_without_newline() {
        let rows = parse("a,b\n1,2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], vec!["1", "2"]);
    }

    #[test]
    fn table_lookup_by_header() {
        let t = Table::from_csv("Key,Summary,Labels,Labels\nP-1,Title,bug,p1\n").unwrap();
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.col(&["key"]), Some(0));
        assert_eq!(t.cols(&["labels"]), vec![2, 3]);
        let row = &t.rows[0];
        assert_eq!(t.get(row, t.col(&["summary"])), "Title");
        assert_eq!(t.get(row, t.col(&["missing"])), "");
    }
}
