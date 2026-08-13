use std::{env, path::Path};

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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ImageEditorCommand;

    #[test]
    fn custom_editor_commands_are_shell_free_and_substitute_paths() {
        let editor = ImageEditorCommand::from_json(
            r#"["image-tool","edit","{input}","--return","{output}"]"#,
        )
        .unwrap();
        let command = editor.command(Path::new("input image.png"), Path::new("edited image.png"));
        let arguments: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            ["edit", "input image.png", "--return", "edited image.png"]
        );
    }

    #[test]
    fn editor_contract_requires_input_and_output_placeholders() {
        assert!(ImageEditorCommand::from_json(r#"["editor","{input}"]"#).is_err());
        assert!(ImageEditorCommand::from_json(r#"["editor","{output}"]"#).is_err());
        assert!(ImageEditorCommand::from_json(r#""editor --in {input}""#).is_err());
    }

    #[test]
    fn default_adapter_returns_satty_save_and_copy_through_the_output() {
        let command = ImageEditorCommand::default()
            .command(Path::new("input image.png"), Path::new("edited image.png"));
        let arguments: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        for pair in [
            ["--filename", "input image.png"],
            ["--output-filename", "edited image.png"],
            ["--actions-on-enter", "save-to-file"],
            ["--actions-on-escape", "exit"],
            ["--actions-on-right-click", "save-to-file"],
            ["--copy-command", "cat >/dev/null"],
        ] {
            assert!(arguments.windows(2).any(|window| window == pair));
        }
        assert!(arguments.iter().any(|value| value == "--save-after-copy"));
        assert!(arguments.iter().any(|value| value == "--early-exit"));
    }
}
