//! Text cleaning — pure function, no I/O.

/// Normalize: strip BOM, CRLF/CR → LF, trim trailing whitespace per line,
/// collapse runs of blank lines to a single blank line, trim leading/trailing
/// blank lines. Markdown syntax is preserved as-is.
pub fn clean(input: &str) -> String {
    let s = input.strip_prefix('\u{feff}').unwrap_or(input);
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bom_and_normalizes_newlines() {
        let out = clean("\u{feff}a\r\nb\rc");
        assert_eq!(out, "a\nb\nc");
    }

    #[test]
    fn trims_line_trailing_whitespace() {
        assert_eq!(clean("a  \nb\t\n"), "a\nb");
    }

    #[test]
    fn collapses_blank_runs_to_one() {
        assert_eq!(clean("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn trims_leading_and_trailing_blank_lines() {
        assert_eq!(clean("\n\na\n\n"), "a");
    }

    #[test]
    fn keeps_markdown_structure() {
        let md = "# Title\n\n- item\n\n## Sub\n";
        assert_eq!(clean(md), "# Title\n\n- item\n\n## Sub");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(clean(""), "");
    }
}
