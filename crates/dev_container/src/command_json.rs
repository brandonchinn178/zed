use std::process::Output;

use serde::Deserialize;
use smol::process::Command;

use crate::devcontainer_api::DevContainerError;

pub(crate) async fn evaluate_json_command<T>(
    mut command: Command,
) -> Result<Option<T>, DevContainerError>
where
    T: for<'de> Deserialize<'de>,
{
    let output = command.output().await.map_err(|e| {
        log::error!("Error running command {:?}: {e}", command);
        DevContainerError::CommandFailed(command.get_program().display().to_string())
    })?;

    deserialize_json_output(output).map_err(|e| {
        log::error!("Error running command {:?}: {e}", command);
        DevContainerError::CommandFailed(command.get_program().display().to_string())
    })
}

pub(crate) fn deserialize_json_output<T>(output: Output) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        if raw.is_empty() {
            return Ok(None);
        }
        let value = serde_json_lenient::from_str(&raw)
            .map_err(|e| format!("Error deserializing from raw json: {e}"));
        value
    } else {
        let std_err = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Sent non-successful output; cannot deserialize. StdErr: {std_err}"
        ))
    }
}
