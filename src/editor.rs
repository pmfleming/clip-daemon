use std::{env, io, path::Path};

use tokio::process::Command;

const EDITOR_COMMAND_ENV: &str = "CLIP_DAEMON_IMAGE_EDITOR_COMMAND";
const INPUT_PLACEHOLDER: &str = "{input}";
const OUTPUT_PLACEHOLDER: &str = "{output}";

/// A shell-free image-editor adapter. The child must block until editing is
/// complete and write a PNG to `{output}`. No output means cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageEditorCommand {
    argv: Vec<String>,
}

impl ImageEditorCommand {
    pub fn configured() -> Self {
        let Some(value) = env::var_os(EDITOR_COMMAND_ENV) else {
            return Self::default();
        };
        match value
            .into_string()
            .map_err(|_| "value is not UTF-8".to_owned())
            .and_then(|value| Self::from_json(&value))
        {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(
                    variable = EDITOR_COMMAND_ENV,
                    %error,
                    "invalid image editor command; using the default adapter"
                );
                Self::default()
            }
        }
    }

    pub fn from_json(value: &str) -> Result<Self, String> {
        let argv: Vec<String> = serde_json::from_str(value)
            .map_err(|_| "command must be a JSON array of strings".to_owned())?;
        Self::new(argv)
    }

    fn new(argv: Vec<String>) -> Result<Self, String> {
        if argv.first().is_none_or(String::is_empty) {
            return Err("command must name an executable".into());
        }
        for required in [INPUT_PLACEHOLDER, OUTPUT_PLACEHOLDER] {
            if !argv.iter().skip(1).any(|argument| argument == required) {
                return Err(format!("command is missing the {required} argument"));
            }
        }
        Ok(Self { argv })
    }

    pub(crate) async fn run(&self, input: &Path, output: &Path) -> io::Result<()> {
        let mut child = self.command(input, output).spawn()?;
        let _process_group = EditorProcessGroup(
            child
                .id()
                .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
        );
        child
            .wait()
            .await?
            .success()
            .then_some(())
            .ok_or_else(|| io::Error::other("Image editor exited unsuccessfully"))
    }

    pub fn command(&self, input: &Path, output: &Path) -> Command {
        let mut command = Command::new(&self.argv[0]);
        command.kill_on_drop(true);
        command.process_group(0);
        for argument in &self.argv[1..] {
            match argument.as_str() {
                INPUT_PLACEHOLDER => {
                    command.arg(input);
                }
                OUTPUT_PLACEHOLDER => {
                    command.arg(output);
                }
                _ => {
                    command.arg(argument);
                }
            }
        }
        command
    }
}

impl Default for ImageEditorCommand {
    fn default() -> Self {
        Self {
            argv: [
                "satty",
                "--filename",
                INPUT_PLACEHOLDER,
                "--output-filename",
                OUTPUT_PLACEHOLDER,
                "--resize",
                "smart",
                "--early-exit",
                "--actions-on-enter",
                "save-to-file",
                "--actions-on-escape",
                "exit",
                "--actions-on-right-click",
                "save-to-file",
                // Satty's Copy button writes the output but does not race the
                // daemon for ownership of the Wayland clipboard.
                "--save-after-copy",
                "--copy-command",
                "cat >/dev/null",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

struct EditorProcessGroup(Option<rustix::process::Pid>);

impl Drop for EditorProcessGroup {
    fn drop(&mut self) {
        if let Some(process_group) = self.0 {
            let _ =
                rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ImageEditorCommand;

    fn arguments(command: &tokio::process::Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn editor_exit_status_and_output_are_observable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let input = directory.path().join("input.png");
        let output = directory.path().join("output.png");
        std::fs::write(&input, b"input").expect("write input");
        let success = ImageEditorCommand::from_json(
            r#"["sh","-c","cp \"$1\" \"$2\"","editor","{input}","{output}"]"#,
        )
        .expect("success editor");
        success.run(&input, &output).await.expect("editor succeeds");
        assert_eq!(std::fs::read(&output).unwrap(), b"input");
        let failure =
            ImageEditorCommand::from_json(r#"["sh","-c","exit 9","editor","{input}","{output}"]"#)
                .expect("failure editor");
        assert!(failure.run(&input, &output).await.is_err());
    }

    #[test]
    fn editor_commands_require_placeholders_and_substitute_paths_without_a_shell() {
        let editor = ImageEditorCommand::from_json(
            r#"["image-tool","edit","{input}","--return","{output}"]"#,
        )
        .unwrap();
        let arguments =
            arguments(&editor.command(Path::new("input image.png"), Path::new("edited image.png")));
        assert_eq!(
            arguments,
            ["edit", "input image.png", "--return", "edited image.png"]
        );

        for invalid in [
            r#"["editor","{input}"]"#,
            r#"["editor","{output}"]"#,
            r#""editor --in {input}""#,
        ] {
            assert!(ImageEditorCommand::from_json(invalid).is_err(), "{invalid}");
        }
    }
}
