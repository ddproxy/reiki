//! The table scanner the detection plugins share.
//!
//! Each plugin keeps its own `Kind`, its own column-name table and its own rules for whether one
//! cell is a valid value. This module supplies the shape of the input: splitting text into lines,
//! a line into cells, reading headers, INSERT tuples, COPY rows and delimited table rows.

use crate::Hit;

/// One line of the text, with its byte offset.
pub struct Line<'a> {
    pub start: usize,
    pub text: &'a str,
}

/// The `(start, end)` byte range of each cell in `line`, offset by `base`; quotes are excluded.
pub fn split_cells(line: &str, delim: u8, base: usize) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    let mut cells = Vec::new();
    let mut i = 0;
    loop {
        let mut start = i;
        while b.get(start) == Some(&b' ') && delim != b' ' {
            start += 1;
        }
        let (s, e, next);
        if b.get(start) == Some(&b'"') {
            let mut j = start + 1;
            loop {
                match b.get(j) {
                    None => break,
                    Some(b'"') if b.get(j + 1) == Some(&b'"') => j += 2,
                    Some(b'"') => break,
                    Some(_) => j += 1,
                }
            }
            s = start + 1;
            e = j.min(b.len());
            let after = (j + 1).min(b.len());
            next = b[after..]
                .iter()
                .position(|c| *c == delim)
                .map(|p| after + p);
        } else {
            let end = b[i..]
                .iter()
                .position(|c| *c == delim)
                .map_or(b.len(), |p| i + p);
            s = start.min(end);
            let mut trimmed = end;
            while trimmed > s && b[trimmed - 1] == b' ' {
                trimmed -= 1;
            }
            e = trimmed;
            next = (end < b.len()).then_some(end);
        }
        cells.push((base + s, base + e));
        match next {
            Some(p) => i = p + 1,
            None => break,
        }
    }
    cells
}

/// Each line of `text`, with its byte offset.
pub fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        out.push(Line {
            start,
            text: piece.trim_end_matches(['\r', '\n']),
        });
        start += piece.len();
    }
    out
}

/// Reads the `(...)` tuples after VALUES starting at `i`; returns where it stopped.
pub fn tuples<K: Copy>(
    text: &str,
    mut i: usize,
    map: &[Option<K>],
    hits: &mut Vec<Hit<K>>,
    cell_ok: fn(K, &str) -> bool,
) -> usize {
    let b = text.as_bytes();
    loop {
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b',')
        {
            i += 1;
        }
        if b.get(i) != Some(&b'(') {
            return i;
        }
        i += 1;
        let mut column = 0;
        loop {
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            let (s, e, next) = if b.get(i) == Some(&b'\'') {
                let mut j = i + 1;
                loop {
                    match b.get(j) {
                        None => break,
                        Some(b'\'') if b.get(j + 1) == Some(&b'\'') => j += 2,
                        Some(b'\'') => break,
                        Some(_) => j += 1,
                    }
                }
                (i + 1, j.min(b.len()), (j + 1).min(b.len()))
            } else {
                let mut j = i;
                let mut depth = 0usize;
                while let Some(c) = b.get(j) {
                    match c {
                        b'(' => depth += 1,
                        b')' if depth == 0 => break,
                        b')' => depth -= 1,
                        b',' if depth == 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                let mut end = j;
                while end > i && b[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                (i, end, j)
            };
            if let Some(Some(kind)) = map.get(column) {
                if cell_ok(*kind, &text[s..e]) {
                    hits.push(Hit {
                        kind: *kind,
                        start: s,
                        end: e,
                    });
                }
            }
            column += 1;
            i = next;
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b')') => {
                    i += 1;
                    break;
                }
                _ => return i,
            }
        }
    }
}

/// `COPY t (a, b) FROM stdin;` followed by tab-separated rows up to `\.`.
pub fn copy_blocks<K: Copy>(
    text: &str,
    all: &[Line<'_>],
    want: &dyn Fn(K) -> bool,
    column_kind: fn(&str) -> Option<K>,
    cell_ok: fn(K, &str) -> bool,
    hits: &mut Vec<Hit<K>>,
) {
    let mut i = 0;
    while i < all.len() {
        let lower = all[i].text.to_ascii_lowercase();
        let head = lower.trim_start();
        let (Some(open), Some(close)) = (all[i].text.find('('), all[i].text.find(')')) else {
            i += 1;
            continue;
        };
        if !(head.starts_with("copy ") && head.contains("from stdin") && open < close) {
            i += 1;
            continue;
        }
        let map: Vec<Option<K>> = all[i].text[open + 1..close]
            .split(',')
            .map(|name| column_kind(name.trim().trim_matches('"')).filter(|k| want(*k)))
            .collect();
        let mut j = i + 1;
        while j < all.len() && all[j].text != "\\." {
            let cells = split_cells(all[j].text, b'\t', all[j].start);
            if cells.len() == map.len() {
                for (idx, (s, e)) in cells.iter().enumerate() {
                    if let Some(kind) = map[idx] {
                        if cell_ok(kind, &text[*s..*e]) {
                            hits.push(Hit {
                                kind,
                                start: *s,
                                end: *e,
                            });
                        }
                    }
                }
            }
            j += 1;
        }
        i = j + 1;
    }
}

const DELIMITERS: [u8; 4] = *b"\t|,;";
/// A column list is short and holds no `(`; stopping there keeps a body of unclosed INSERTs from
/// rescanning ahead on every occurrence.
const MAX_COLUMN_LIST: usize = 4096;

/// The plugin-specific part of table scanning: which columns matter and which cells are valid.
#[derive(Clone, Copy)]
pub struct TableRules<K> {
    pub column_kind: fn(&str) -> Option<K>,
    pub cell_ok: fn(K, &str) -> bool,
    /// When this predicate accepts a mapped kind, at least half of the header's non-empty cells
    /// must be recognised column names. A catch-all kind accepts almost any cell, so without the
    /// ratio a data row such as `alice,address` would become a header.
    pub catch_all: Option<fn(K) -> bool>,
}

/// Every hit in the tables of `text` whose kind passes `want`.
pub fn scan_tables<K: Copy>(
    text: &str,
    want: &dyn Fn(K) -> bool,
    rules: TableRules<K>,
) -> Vec<Hit<K>> {
    let mut hits = Vec::new();
    let all = lines(text);
    let mut i = 0;
    while i < all.len() {
        let prev = i.checked_sub(1).map(|p| &all[p]);
        let Some((delim, map, width)) = header(&all[i], prev, want, &rules) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < all.len() {
            if all[j].text.trim().is_empty() {
                break;
            }
            // A row of another width is skipped, not treated as the end: a quoted cell that spans
            // lines leaves a short line behind.
            let cells = split_cells(all[j].text, delim, all[j].start);
            if cells.len() == width {
                for (idx, (s, e)) in cells.iter().enumerate() {
                    if let Some(kind) = map[idx] {
                        if (rules.cell_ok)(kind, &text[*s..*e]) {
                            hits.push(Hit {
                                kind,
                                start: *s,
                                end: *e,
                            });
                        }
                    }
                }
            }
            j += 1;
        }
        i = j;
    }
    sql(text, want, &mut hits, &rules);
    copy_blocks(
        text,
        &all,
        want,
        rules.column_kind,
        rules.cell_ok,
        &mut hits,
    );
    hits
}

/// True when `delim` starts a table on this line: the line is the first one in the document or
/// follows a blank line or a line without that delimiter.
fn starts_table(prev: Option<&Line<'_>>, delim: u8) -> bool {
    match prev {
        None => true,
        Some(p) => p.text.trim().is_empty() || split_cells(p.text, delim, 0).len() < 2,
    }
}

/// The delimiter, the kind of each column and the column count when `line` is a table header.
#[allow(clippy::type_complexity)]
fn header<K: Copy>(
    line: &Line<'_>,
    prev: Option<&Line<'_>>,
    want: &dyn Fn(K) -> bool,
    rules: &TableRules<K>,
) -> Option<(u8, Vec<Option<K>>, usize)> {
    for delim in DELIMITERS {
        let cells = split_cells(line.text, delim, 0);
        if cells.len() < 2 || !starts_table(prev, delim) {
            continue;
        }
        let known = cells
            .iter()
            .filter(|(s, e)| (rules.column_kind)(&line.text[*s..*e]).is_some())
            .count();
        let nonempty = cells
            .iter()
            .filter(|(s, e)| !line.text[*s..*e].is_empty())
            .count();
        let map: Vec<Option<K>> = cells
            .iter()
            .map(|(s, e)| (rules.column_kind)(&line.text[*s..*e]).filter(|k| want(*k)))
            .collect();
        if !map.iter().any(Option::is_some) {
            continue;
        }
        if rules
            .catch_all
            .is_some_and(|catch_all| map.iter().flatten().any(|kind| catch_all(*kind)))
            && known * 2 < nonempty
        {
            continue;
        }
        return Some((delim, map, cells.len()));
    }
    None
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'"' | b'`' | b'.' | b'[' | b']')
}

/// `INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');`
fn sql<K: Copy>(
    text: &str,
    want: &dyn Fn(K) -> bool,
    hits: &mut Vec<Hit<K>>,
    rules: &TableRules<K>,
) {
    let b = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("insert into") {
        let mut i = from + p + "insert into".len();
        from = i;
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        while b.get(i).is_some_and(|c| is_ident(*c)) {
            i += 1;
        }
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        if b.get(i) != Some(&b'(') {
            continue;
        }
        let Some(close) = b[i + 1..]
            .iter()
            .take(MAX_COLUMN_LIST)
            .take_while(|c| **c != b'(')
            .position(|c| *c == b')')
            .map(|q| i + 1 + q)
        else {
            continue;
        };
        let map: Vec<Option<K>> = text[i + 1..close]
            .split(',')
            .map(|name| {
                (rules.column_kind)(name.trim().trim_matches(['"', '`', '[', ']']))
                    .filter(|k| want(*k))
            })
            .collect();
        i = close + 1;
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        if !lower[i..].starts_with("values") {
            continue;
        }
        i += "values".len();
        if map.iter().all(Option::is_none) {
            continue;
        }
        i = tuples(text, i, &map, hits, rules.cell_ok);
        from = from.max(i);
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_blocks, lines, scan_tables, split_cells, tuples, TableRules};

    #[test]
    fn split_cells_splits_and_trims() {
        assert_eq!(split_cells("a, b ,c", b',', 0), [(0, 1), (3, 4), (6, 7)]);
        assert_eq!(split_cells("\"a, b\",c", b',', 0), [(1, 5), (7, 8)]);
        assert_eq!(split_cells("\"a\"\"b\",c", b',', 0), [(1, 5), (7, 8)]);
        assert_eq!(split_cells("x,y", b',', 10), [(10, 11), (12, 13)]);
    }

    #[test]
    fn lines_keep_offsets_and_drop_line_endings() {
        let all = lines("a\nbb\r\nccc\n");
        let got: Vec<_> = all.iter().map(|l| (l.start, l.text)).collect();
        assert_eq!(got, [(0, "a"), (2, "bb"), (6, "ccc")]);
    }

    #[test]
    fn tuples_read_values_into_the_mapped_columns() {
        let text = "INSERT INTO t (id, iban) VALUES (1, 'GB82WEST12345698765432');";
        let start = text.find("VALUES").unwrap() + "VALUES".len();
        let map = [Some(1u8), Some(2u8)];
        let mut hits = Vec::new();
        let at = tuples(text, start, &map, &mut hits, |kind, value| {
            value == "GB82WEST12345698765432" && kind == 2
        });
        assert_eq!(at, text.len() - 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].start..hits[0].end], "GB82WEST12345698765432");
    }

    #[test]
    fn copy_blocks_reads_tab_rows_up_to_the_period() {
        let text = "COPY t (id, iban) FROM stdin;\n1\tGB82WEST12345698765432\n\\.\n";
        let all = lines(text);
        let mut hits = Vec::new();
        copy_blocks(
            text,
            &all,
            &|kind| kind == 2,
            |name| name.eq("iban").then_some(2),
            |kind, value| kind == 2 && value == "GB82WEST12345698765432",
            &mut hits,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].start..hits[0].end], "GB82WEST12345698765432");
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum TestKind {
        Ssn,
        Dob,
        Column,
        Iban,
        Swift,
        Cusip,
        Isin,
        Crypto,
        Account,
    }

    fn column_kind(name: &str) -> Option<TestKind> {
        let n: String = name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Some(match n.as_str() {
            "ssn" => TestKind::Ssn,
            "dob" | "birthdate" => TestKind::Dob,
            "firstname" | "lastname" | "address" => TestKind::Column,
            "iban" => TestKind::Iban,
            "swift" | "bic" => TestKind::Swift,
            "cusip" => TestKind::Cusip,
            "isin" => TestKind::Isin,
            "wallet" => TestKind::Crypto,
            "account" => TestKind::Account,
            _ => return None,
        })
    }

    fn cell_ok(kind: TestKind, value: &str) -> bool {
        match kind {
            TestKind::Ssn => {
                let b = value.as_bytes();
                b.iter().filter(|c| c.is_ascii_digit()).count() == 9
                    && (b.len() == 9 || (b.len() == 11 && b[3] == b'-' && b[6] == b'-'))
            }
            TestKind::Dob => matches!(value, "02/03/1980" | "1975-12-31" | "1980-02-03"),
            TestKind::Column => {
                value.bytes().any(|b| b.is_ascii_alphanumeric())
                    && !matches!(value, "string" | "str" | "ok" | "seen" | "x" | "N/A")
            }
            TestKind::Iban => value
                .bytes()
                .filter(|b| !b.is_ascii_whitespace())
                .eq("GB82WEST12345698765432".bytes()),
            TestKind::Swift => value == "DEUTDEFF",
            TestKind::Cusip => value == "037833100",
            TestKind::Isin => value == "US0378331005",
            TestKind::Crypto => value == "0x52908400098527886E0F7030069857D2E4169EE7",
            TestKind::Account => {
                (6..=17).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
            }
        }
    }

    fn rules(catch_all: bool) -> TableRules<TestKind> {
        TableRules {
            column_kind,
            cell_ok,
            catch_all: catch_all.then_some(|kind| kind == TestKind::Column),
        }
    }

    fn found(text: &str, catch_all: bool) -> Vec<TestKind> {
        let mut kinds: Vec<_> = scan_tables(text, &|_| true, rules(catch_all))
            .into_iter()
            .map(|h| h.kind)
            .collect();
        kinds.sort();
        kinds
    }

    fn values(text: &str, catch_all: bool) -> Vec<&str> {
        scan_tables(text, &|_| true, rules(catch_all))
            .into_iter()
            .map(|h| &text[h.start..h.end])
            .collect()
    }

    #[test]
    fn scan_tables_gives_bare_cells_the_meaning_of_their_header() {
        assert_eq!(
            found(
                "id,ssn,first_name,dob\n1,123456789,Jane,02/03/1980\n2,234-56-7890,Joe,1975-12-31\n",
                true
            ),
            [
                TestKind::Ssn,
                TestKind::Ssn,
                TestKind::Dob,
                TestKind::Dob,
                TestKind::Column,
                TestKind::Column
            ]
        );
        assert_eq!(found("id,ssn\n1,123456789\n", true), [TestKind::Ssn]);
        assert_eq!(
            found("id,iban\n1,GB82WEST12345698765432\n", false),
            [TestKind::Iban]
        );
        assert_eq!(found("id,bic\n1,DEUTDEFF\n", false), [TestKind::Swift]);
        assert_eq!(
            found("cusip\tisin\n037833100\tUS0378331005\n", false),
            [TestKind::Cusip, TestKind::Isin]
        );
        assert_eq!(
            found(
                "wallet|note\n0x52908400098527886E0F7030069857D2E4169EE7|x\n",
                false
            ),
            [TestKind::Crypto]
        );
        assert_eq!(
            found("id,account\n1,000123456789\n", false),
            [TestKind::Account]
        );
    }

    #[test]
    fn scan_tables_handles_delimiters_quotes_markdown_and_quoted_sql_cells() {
        assert_eq!(found("ssn\tname\n123456789\tx\n", true), [TestKind::Ssn]);
        assert_eq!(
            found("mrn|dob\nA1234567|02/03/1980\n", true),
            [TestKind::Dob]
        );
        assert_eq!(found("ssn;city\n123456789;x\n", true), [TestKind::Ssn]);
        assert_eq!(
            found(
                "| ssn | last_name |\n|---|---|\n| 123456789 | Doe |\n",
                true
            ),
            [TestKind::Ssn, TestKind::Column]
        );
        assert_eq!(
            found("last_name,ssn\n\"Doe, Jane\",123456789\n", true),
            [TestKind::Ssn, TestKind::Column]
        );
        assert_eq!(
            found("\"iban\",note\n\"GB82 WEST 1234 5698 7654 32\",x\n", false),
            [TestKind::Iban]
        );
    }

    #[test]
    fn scan_tables_reads_sql_inserts_and_copy_blocks() {
        assert_eq!(
            found(
                "INSERT INTO patients (id, first_name, ssn, dob) VALUES (1, 'Jane', '123456789', '1980-02-03'), (2, 'Joe', '234567890', NULL);",
                true
            ),
            [
                TestKind::Ssn,
                TestKind::Ssn,
                TestKind::Dob,
                TestKind::Column,
                TestKind::Column
            ]
        );
        assert_eq!(
            found(
                "insert into t (\"ssn\") values ('1''2'),('123456789');",
                true
            ),
            [TestKind::Ssn]
        );
        assert_eq!(
            found(
                "INSERT INTO t (id, iban, swift) VALUES (1, 'GB82WEST12345698765432', 'DEUTDEFF'), (2, 'N/A', NULL);",
                false
            ),
            [TestKind::Iban, TestKind::Swift]
        );
        assert_eq!(
            found(
                "COPY patients (id, ssn, last_name) FROM stdin;\n1\t123456789\tDoe\n2\t234567890\tRoe\n\\.\n",
                true
            ),
            [
                TestKind::Ssn,
                TestKind::Ssn,
                TestKind::Column,
                TestKind::Column
            ]
        );
        assert_eq!(
            found(
                "COPY t (id, isin) FROM stdin;\n1\tUS0378331005\n\\.\n",
                false
            ),
            [TestKind::Isin]
        );
    }

    #[test]
    fn scan_tables_header_guard_and_row_boundaries() {
        assert!(
            found("name,notes\nalice,address\nbob,phone\n", true).is_empty(),
            "a data row must not become a header"
        );
        assert!(
            found("name,notes\nalice,address\nbob,phone\n", false).is_empty(),
            "the table boundary applies without a catch-all kind too"
        );
        assert_eq!(
            found(
                "Some notes about the export.\n\nid,ssn,notes\n1,123456789,ok\n",
                true
            ),
            [TestKind::Ssn]
        );
        assert_eq!(
            found(
                "id,ssn,first_name,dob,notes\n1,123456789,Jane,02/03/1980,seen\n",
                true
            ),
            [TestKind::Ssn, TestKind::Dob, TestKind::Column],
            "three of five header cells are known, so the table is read"
        );
        assert!(found("first_name,last_name\nstring,string\n", true).is_empty());
        assert!(found("ssn,notes\nN/A,ok\n,\n", true).is_empty());
        assert!(found("id,price,qty\n1,9.99,3\n", false).is_empty());
        assert!(found("iban,isin\nN/A,N/A\n", false).is_empty());
        assert!(found("INSERT INTO t (a, b) VALUES (1, 2);", true).is_empty());
        assert!(found("INSERT INTO t VALUES ('123456789');", true).is_empty());

        assert_eq!(
            values("ssn,last_name\n123456789,Doe\nx,y,z\n234567890,Roe\n", true),
            ["123456789", "Doe", "234567890", "Roe"]
        );
        assert_eq!(
            values(
                "ssn,last_name\n123456789,\"Doe\nJane\"\n234567890,Roe\n",
                true
            ),
            ["123456789", "Doe", "234567890", "Roe"]
        );
        assert_eq!(
            values("ssn,last_name\n123456789,Doe\n\n234567890,Roe\n", true),
            ["123456789", "Doe"]
        );
        let iban = "GB82WEST12345698765432";
        assert_eq!(
            values(
                &format!("iban,note\n{iban},x\nfoo,bar,baz\n{iban},y\n"),
                false
            ),
            [iban, iban]
        );
        assert_eq!(
            values(&format!("iban,note\n{iban},\"a\nb\"\n{iban},y\n"), false),
            [iban, iban]
        );
        assert_eq!(
            values(&format!("iban,note\n{iban},x\n\n{iban},y\n"), false),
            [iban]
        );
    }

    #[test]
    fn scan_tables_accepts_malformed_input() {
        for text in [
            "ssn,\n\"",
            "ssn,x\n\"unterminated,1",
            "insert into t (ssn) values (",
            "insert into t (ssn",
            "copy t (ssn) from stdin;\n",
            "|\n|",
            ",,,,\n,,,,",
            "insert into t (ssn) values ('",
            "GB",
            "GB82WEST",
            "0x",
        ] {
            let _ = scan_tables(text, &|_| true, rules(true));
            let _ = scan_tables(text, &|_| true, rules(false));
        }
    }
}
