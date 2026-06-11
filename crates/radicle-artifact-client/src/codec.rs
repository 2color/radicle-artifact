//! Line framing shared by the [`sync`](crate::sync) and
//! [`tokio`](crate::tokio) transports: one JSON value per `\n`-terminated
//! line, requests are [`Command`]s, responses are [`CommandResult`] or
//! [`StreamEvent`] frames.

use serde::de::DeserializeOwned;

use radicle_artifact_core::protocol::{Command, CommandResult, StreamEvent};

use crate::ClientError;

/// Encode a command as a single newline-terminated JSON line.
pub fn encode_command(cmd: &Command) -> Result<String, ClientError> {
    let mut line = serde_json::to_string(cmd)?;
    line.push('\n');
    Ok(line)
}

/// Decode a one-shot response line into the command's payload.
pub fn decode_result<T: DeserializeOwned>(line: &str) -> Result<T, ClientError> {
    let parsed: CommandResult<T> = serde_json::from_str(line.trim_end())?;
    match parsed {
        CommandResult::Okay(v) => Ok(v),
        CommandResult::Error(e) => Err(ClientError::Remote(e)),
    }
}

/// Decode one frame of a streaming response.
pub fn decode_stream_event<T: DeserializeOwned>(line: &str) -> Result<StreamEvent<T>, ClientError> {
    Ok(serde_json::from_str(line.trim_end())?)
}

#[cfg(test)]
mod tests {
    use radicle_artifact_core::protocol::{CommandError, ErrorCode, FetchProgress, StreamEvent};

    use super::*;

    #[test]
    fn command_round_trip() {
        let line = encode_command(&Command::Alive).unwrap();
        assert_eq!(line, "{\"command\":\"alive\"}\n");
    }

    #[test]
    fn result_okay_and_error() {
        let v: u32 = decode_result(r#"{"okay": 7}"#).unwrap();
        assert_eq!(v, 7);

        let err = decode_result::<u32>(r#"{"error": {"code": "cid-mismatch", "message": "boom"}}"#)
            .unwrap_err();
        assert!(matches!(
            err,
            ClientError::Remote(CommandError {
                code: ErrorCode::CidMismatch,
                ..
            })
        ));
    }

    #[test]
    fn stream_event_frames() {
        let ev: StreamEvent<u32> =
            decode_stream_event(r#"{"progress": {"kind": "connecting"}}"#).unwrap();
        assert_eq!(ev, StreamEvent::Progress(FetchProgress::Connecting));

        let ev: StreamEvent<u32> = decode_stream_event(r#"{"okay": 7}"#).unwrap();
        assert_eq!(ev, StreamEvent::Okay(7));
    }
}
