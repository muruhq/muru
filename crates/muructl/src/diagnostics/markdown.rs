//! Markdown that an issue tracker shows as it was meant: every value that
//! came from the system goes into a code span or a fenced block, so that
//! nothing in it is read as markup. Only CommonMark and pipe tables are
//! written, with no HTML.

/// A value as a code span. The delimiter is longer than any run of
/// backticks inside, so the value cannot close the span, and a line
/// break becomes a space, since a span sits on one line. Inside a span a
/// tracker links no `@name` or `#123`.
pub fn code(value: &str) -> String {
    if value.trim().is_empty() {
        return "-".to_owned();
    }

    let value: String = value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let fence = "`".repeat(longest_run(&value, '`') + 1);

    // A value that starts or ends with a backtick is set off from the
    // delimiter by a space, which the span drops again.

    if value.starts_with('`') || value.ends_with('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

/// A value as a code span in a table cell, where a pipe would end the
/// cell: each is written `\|`, which a table reads as the pipe itself,
/// in a code span too.
///
/// A table takes a backslash before a pipe as escaping it, and two as a
/// backslash followed by the end of the cell. A value with an odd number
/// of backslashes before a pipe would so leave its pipe bare once the
/// escape is added, so it is given one more: the cell then shows one
/// backslash too many there, and the table keeps its shape.
pub fn cell(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut backslashes = 0;

    for c in value.chars() {
        if c == '|' {
            if backslashes % 2 == 1 {
                escaped.push('\\');
            }

            escaped.push('\\');
        }

        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
        escaped.push(c);
    }

    code(&escaped)
}

/// Lines as a fenced block, the fence longer than any run of backticks
/// in them, so that no line closes it.
pub fn fenced(lines: &[String]) -> String {
    let longest = lines
        .iter()
        .map(|line| longest_run(line, '`'))
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let mut out = format!("{fence}text\n");

    for line in lines {
        for part in line.split(['\n', '\r']) {
            out.push_str(part);
            out.push('\n');
        }
    }

    out.push_str(&fence);
    out.push('\n');
    out
}

/// A table: its header, then a row for each of `rows`, whose cells are
/// already written.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = format!("| {} |\n", header.join(" | "));
    out.push_str(&format!("|{}\n", " --- |".repeat(header.len())));

    for row in rows {
        out.push_str(&format!("| {} |\n", row.join(" | ")));
    }

    out
}

/// The longest run of `of` in `text`.
fn longest_run(text: &str, of: char) -> usize {
    let mut longest = 0;
    let mut run = 0;

    for c in text.chars() {
        run = if c == of { run + 1 } else { 0 };
        longest = longest.max(run);
    }

    longest
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The text of a Markdown document outside its code spans and fenced
    /// blocks: what a tracker would read as markup.
    pub fn outside_code(markdown: &str) -> String {
        let mut out = String::new();
        let mut fence: Option<usize> = None;

        for line in markdown.lines() {
            let ticks = line.len() - line.trim_start_matches('`').len();

            match fence {
                Some(open) => {
                    if ticks >= open && line.trim_start_matches('`').is_empty() {
                        fence = None;
                    }

                    continue;
                }

                // A fence opens with three or more backticks and a word
                // that holds none; with one, the line is a code span.
                None if ticks >= 3 && !line[ticks..].contains('`') => {
                    fence = Some(ticks);
                    continue;
                }
                None => {}
            }

            // Within a line, a run of backticks opens a span that the
            // next run of the same length closes.

            let mut rest = line;

            while let Some(start) = rest.find('`') {
                out.push_str(&rest[..start]);
                let after = &rest[start..];
                let length = after.len() - after.trim_start_matches('`').len();
                let delimiter = "`".repeat(length);
                let body = &after[length..];

                let closed = body.match_indices(&delimiter).find(|(at, _)| {
                    !body[..*at].ends_with('`') && !body[at + length..].starts_with('`')
                });

                match closed {
                    Some((at, _)) => rest = &body[at + length..],
                    None => {
                        out.push_str(after);
                        rest = "";
                    }
                }
            }

            out.push_str(rest);
            out.push('\n');
        }

        assert!(fence.is_none(), "a fenced block was left open");
        out
    }

    /// The cells of a table row, split as a tracker splits them: at each
    /// pipe that no backslash escapes.
    pub fn cells(row: &str) -> Vec<String> {
        let mut cells = Vec::new();
        let mut cell = String::new();
        let mut chars = row.trim().chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek().is_some_and(|next| "\\|".contains(*next)) => {
                    cell.push(c);
                    cell.extend(chars.next());
                }
                '|' => cells.push(std::mem::take(&mut cell)),
                _ => cell.push(c),
            }
        }

        cells.push(cell);

        // A row starts and ends with a pipe, which bound no cell.

        cells[1..cells.len() - 1]
            .iter()
            .map(|cell| cell.trim().to_owned())
            .collect()
    }

    #[test]
    fn a_code_span_outlasts_the_backticks_in_its_value() {
        assert_eq!(code("plain"), "`plain`");
        assert_eq!(code("a `b` c"), "``a `b` c``");
        assert_eq!(code("a ``` b"), "````a ``` b````");
        assert_eq!(code("`edge`"), "`` `edge` ``");
        assert_eq!(code("two\nlines"), "`two lines`");
        assert_eq!(code(""), "-");
        assert_eq!(code("  "), "-");

        // Whatever the value, nothing of it is left outside the span.

        for value in [
            "a `b` c",
            "``",
            "x ``` y `` z `",
            "@alice fixes #123",
            "**bold** <b>",
        ] {
            let span = code(value);
            assert_eq!(outside_code(&span).trim(), "", "{value:?} as {span:?}");
        }
    }

    #[test]
    fn a_pipe_in_a_cell_does_not_end_the_cell() {
        assert_eq!(cell("a|b"), "`a\\|b`");
        assert_eq!(cell("a||b"), "`a\\|\\|b`");

        // One backslash before the pipe would cancel the escape, so the
        // value is given another; two leave it standing.

        assert_eq!(cell("a\\|b"), "`a\\\\\\|b`");
        assert_eq!(cell("a\\\\|b"), "`a\\\\\\|b`");

        for value in ["a|b", "|", "a\\|b", "a\\\\|b", "x | y | z", "\\", "end\\"] {
            let row = table(&["One", "Two"], &[vec![cell(value), cell("after")]]);
            let last = row.lines().last().unwrap();
            let found = cells(last);
            assert_eq!(found.len(), 2, "{value:?} as {last:?}");
            assert_eq!(found[1], "`after`");
        }
    }

    #[test]
    fn a_fence_outlasts_the_backticks_in_its_lines() {
        assert_eq!(fenced(&["one".to_owned()]), "```text\none\n```\n");

        let lines = vec![
            "```".to_owned(),
            "a ```` b".to_owned(),
            "@alice #123".to_owned(),
        ];
        let block = fenced(&lines);
        assert!(block.starts_with("`````text\n"));
        assert!(block.ends_with("\n`````\n"));
        assert_eq!(outside_code(&block), "");
    }

    #[test]
    fn a_table_has_a_row_for_each_of_its_rows() {
        assert_eq!(
            table(&["A", "B"], &[vec!["`1`".to_owned(), "`2`".to_owned()]]),
            "| A | B |\n| --- | --- |\n| `1` | `2` |\n"
        );
    }
}
