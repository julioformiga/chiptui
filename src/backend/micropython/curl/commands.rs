//! `curl` command construction.
//!
//! Same reasoning as `micropython::commands`/`esptool::commands`: every
//! invocation is built here, so an upstream flag change is a one-file fix.

use std::path::Path;

use crate::process::Command;

pub const PROGRAM: &str = "curl";

/// Format string for `download_file`'s `-w` output, parsed by
/// [`super::parse::parse_download_summary`]. One line, space-separated:
/// the HTTP status code and the number of bytes written.
const WRITE_OUT_FORMAT: &str = "%{http_code} %{size_download}";

/// `curl -sS -L <url>` --- fetches `url` and streams the body to stdout,
/// accumulated across [`crate::process::ProcessEvent::Line`] the same way
/// [`crate::flash::FlashPanel`] already accumulates `esptool`'s stdout.
/// `-S` keeps error messages on stderr even with `-s` silencing progress;
/// `-L` follows redirects.
pub fn fetch_page(url: &str) -> Command {
    Command::new(PROGRAM).arg("-sS").arg("-L").arg(url)
}

/// `curl -sS -L -f -o <dest> -w "<format>" <url>` --- downloads `url`
/// straight to `dest`, never through stdout (so a binary firmware image is
/// never routed through the line-oriented, lossy-UTF8 log pump). `-f` makes
/// curl itself fail on an HTTP error status, so a 404 becomes an ordinary
/// [`crate::process::Outcome::Failed`] with no separate status-code check
/// needed; the one `-w` line on stdout carries the confirmed status and byte
/// count for the success message.
pub fn download_file(url: &str, dest: &Path) -> Command {
    Command::new(PROGRAM)
        .arg("-sS")
        .arg("-L")
        .arg("-f")
        .arg("-o")
        .arg(dest.to_string_lossy().into_owned())
        .arg("-w")
        .arg(WRITE_OUT_FORMAT)
        .arg(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_page_is_silent_but_still_reports_errors() {
        let command = fetch_page("https://micropython.org/download/?mcu=esp32");
        assert_eq!(
            command.to_string(),
            "curl -sS -L https://micropython.org/download/?mcu=esp32"
        );
    }

    #[test]
    fn download_writes_straight_to_the_destination_file() {
        let command = download_file(
            "https://micropython.org/resources/firmware/ESP32_GENERIC-v1.28.0.bin",
            Path::new("/tmp/project/ESP32_GENERIC-v1.28.0.bin"),
        );
        assert_eq!(
            command.to_string(),
            "curl -sS -L -f -o /tmp/project/ESP32_GENERIC-v1.28.0.bin -w \"%{http_code} %{size_download}\" https://micropython.org/resources/firmware/ESP32_GENERIC-v1.28.0.bin"
        );
    }

    #[test]
    fn a_destination_with_spaces_is_one_argument() {
        let command = download_file("https://example.org/f.bin", Path::new("/tmp/my dir/f.bin"));
        assert!(
            command
                .args_slice()
                .contains(&"/tmp/my dir/f.bin".to_string())
        );
    }
}
