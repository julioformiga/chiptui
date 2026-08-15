//! Structured command description.
//!
//! `AGENTS.md` §5 and `SPEC.md` §12: commands are built as a program plus an
//! argument vector and executed directly. Nothing here ever reaches a shell, so
//! a filename containing spaces, quotes or `;` is just an argument.

use std::ffi::OsStr;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
}

impl Command {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Adds one environment variable, overriding any inherited value for
    /// that key when the command runs. The structured counterpart of a
    /// shell's `KEY=value cmd`: no string is ever handed to a shell, and the
    /// pair stays visible in the log's rendering of the command.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_env(key, value);
        self
    }

    /// Adds several environment variables at once, last write winning on a
    /// repeated key.
    #[must_use]
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (key, value) in envs {
            self.set_env(key, value);
        }
        self
    }

    fn set_env(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        match self.env.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.env.push((key, value)),
        }
    }

    /// Replaces the executable, keeping the arguments.
    ///
    /// Lets a caller point at a specific binary instead of relying on `PATH` --
    /// what `SPEC.md` §13's `[tools]` overrides will use, and what tests use to
    /// substitute a fake.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args_slice(&self) -> &[String] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    /// The environment overrides, in insertion order. Read by the event
    /// loop to run an *interactive* child (`west build -t menuconfig`) with
    /// the same environment the piped commands get --- `to_std` cannot be
    /// used there, since it captures stdio.
    pub fn envs_slice(&self) -> &[(String, String)] {
        &self.env
    }

    /// Builds the standard-library command with both pipes captured.
    pub(crate) fn to_std(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command
            .args(self.args.iter().map(OsStr::new))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

/// Renders the command for the log pane.
///
/// Quoting here is for human readability only --- this string is never parsed
/// or executed. Environment overrides lead the line the way the equivalent
/// shell spelling would, so what the log shows is the environment the command
/// really ran with (west's `ZEPHYR_BASE`, for one).
impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (key, value) in &self.env {
            write!(f, "{key}={value} ")?;
        }
        f.write_str(&self.program)?;
        for arg in &self.args {
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                write!(f, " \"{arg}\"")?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_program_with_arguments() {
        let command = Command::new("mpremote")
            .arg("fs")
            .args(["--no-verbose", "ls", ":/"]);

        assert_eq!(command.program(), "mpremote");
        assert_eq!(command.args_slice(), ["fs", "--no-verbose", "ls", ":/"]);
        assert_eq!(command.to_string(), "mpremote fs --no-verbose ls :/");
    }

    #[test]
    fn display_quotes_only_for_readability() {
        let command = Command::new("mpremote")
            .arg("fs")
            .arg("cat")
            .arg(":/my file.py");
        assert_eq!(command.to_string(), "mpremote fs cat \":/my file.py\"");
    }

    #[test]
    fn shell_metacharacters_stay_inside_one_argument() {
        // The whole point of the structured form: this is a filename, not a
        // command separator.
        let command = Command::new("mpremote")
            .arg("fs")
            .arg("rm")
            .arg(":/a;rm -rf b");
        assert_eq!(command.args_slice()[2], ":/a;rm -rf b");
        assert_eq!(command.args_slice().len(), 3);
    }

    #[test]
    fn working_directory_is_optional() {
        assert_eq!(Command::new("west").cwd(), None);
        let command = Command::new("west").current_dir("/home/dev/app");
        assert_eq!(command.cwd(), Some(&PathBuf::from("/home/dev/app")));
    }

    #[test]
    fn env_overrides_are_set_and_displayed_first() {
        let command = Command::new("west")
            .arg("build")
            .env("ZEPHYR_BASE", "/ws/zephyr")
            .env("PATH", "/ws/.venv/bin:/usr/bin");
        assert_eq!(
            command.envs_slice(),
            [
                ("ZEPHYR_BASE".to_string(), "/ws/zephyr".to_string()),
                ("PATH".to_string(), "/ws/.venv/bin:/usr/bin".to_string()),
            ]
        );
        assert_eq!(
            command.to_string(),
            "ZEPHYR_BASE=/ws/zephyr PATH=/ws/.venv/bin:/usr/bin west build"
        );
    }

    #[test]
    fn a_repeated_env_key_is_overwritten_not_duplicated() {
        let command = Command::new("west")
            .env("ZEPHYR_BASE", "/old")
            .env("ZEPHYR_BASE", "/new");
        assert_eq!(command.envs_slice().len(), 1);
        assert_eq!(command.envs_slice()[0].1, "/new");
    }
}
