//! Pieces the columnar scanners of the detection plugins share.
//!
//! Each plugin keeps its own `Kind`, its own column-name table and its own rules for whether one
//! cell is a valid value. What they share is the shape of the input: splitting text into lines,
//! a line into cells, and reading the `(...)` tuples of an INSERT and the rows of a COPY block.

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

#[cfg(test)]
mod tests {
    use super::{copy_blocks, lines, split_cells, tuples};

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
}
