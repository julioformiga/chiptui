//! Parsers for `curl` output.

/// The final line `curl::commands::download_file`'s `-w` format prints to
/// stdout: `"<http_code> <bytes written>"`.
pub struct DownloadSummary {
    pub http_code: u16,
    pub bytes: u64,
}

/// Reads the last non-blank line of `stdout` as a [`DownloadSummary`].
/// Tolerant by construction, same spirit as `esptool::parse`: curl's `-w`
/// output is the one line we asked for, but scanning from the end rather
/// than assuming there is exactly one line survives any incidental output
/// ahead of it.
pub fn parse_download_summary(stdout: &str) -> Option<DownloadSummary> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut parts = line.split_whitespace();
    let http_code = parts.next()?.parse().ok()?;
    let bytes = parts.next()?.parse().ok()?;
    Some(DownloadSummary { http_code, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_write_out_line() {
        let summary = parse_download_summary("200 174352\n").unwrap();
        assert_eq!(summary.http_code, 200);
        assert_eq!(summary.bytes, 174352);
    }

    #[test]
    fn ignores_blank_trailing_lines() {
        let summary = parse_download_summary("200 174352\n\n").unwrap();
        assert_eq!(summary.http_code, 200);
    }

    #[test]
    fn malformed_output_yields_nothing() {
        assert!(parse_download_summary("").is_none());
        assert!(parse_download_summary("not a summary line").is_none());
        assert!(parse_download_summary("200").is_none());
    }
}
