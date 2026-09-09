use std::{
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
};

use thiserror::Error;

use crate::{AuthToken, MAX_MESSAGE_BYTES, RequestEnvelope, ResponseEnvelope};

/// Blocking client for one request per local Unix-socket connection.
#[derive(Clone)]
pub struct LocalClient {
    auth_token: AuthToken,
}

impl LocalClient {
    /// Reads the installation token from its owner-only file.
    pub fn from_token_file(path: impl AsRef<Path>) -> Result<Self, TransportError> {
        let value = fs::read_to_string(path)?;
        let auth_token = AuthToken::new(value.trim().to_owned())?;
        Ok(Self { auth_token })
    }

    #[must_use]
    pub fn new(auth_token: AuthToken) -> Self {
        Self { auth_token }
    }

    pub fn send(
        &self,
        socket_path: impl AsRef<Path>,
        command: crate::Command,
    ) -> Result<ResponseEnvelope, TransportError> {
        let request = RequestEnvelope::new(self.auth_token.clone(), command);
        let expected_request_id = request.request_id;
        let mut stream = UnixStream::connect(socket_path)?;
        write_message(&mut stream, &request)?;
        stream.shutdown(Shutdown::Write)?;
        let response: ResponseEnvelope = read_message(&mut stream)?;
        if response.request_id != expected_request_id {
            return Err(TransportError::MismatchedRequestId);
        }
        Ok(response)
    }
}

pub fn write_message(
    writer: &mut impl Write,
    value: &impl serde::Serialize,
) -> Result<(), TransportError> {
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err(TransportError::MessageTooLarge {
            maximum: MAX_MESSAGE_BYTES,
        });
    }
    writer.write_all(&encoded)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<T, TransportError> {
    let mut encoded = Vec::new();
    reader
        .take((MAX_MESSAGE_BYTES + 2) as u64)
        .read_to_end(&mut encoded)?;
    if encoded.len() > MAX_MESSAGE_BYTES + 1 {
        return Err(TransportError::MessageTooLarge {
            maximum: MAX_MESSAGE_BYTES,
        });
    }
    if encoded.last() == Some(&b'\n') {
        encoded.pop();
    }
    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err(TransportError::MessageTooLarge {
            maximum: MAX_MESSAGE_BYTES,
        });
    }
    Ok(serde_json::from_slice(&encoded)?)
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("local transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("local protocol encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] crate::ProtocolValidationError),
    #[error("message exceeds the {maximum}-byte local API limit")]
    MessageTooLarge { maximum: usize },
    #[error("response request ID does not match the command")]
    MismatchedRequestId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, RequestEnvelope};

    #[test]
    fn request_round_trips_through_bounded_wire_format() {
        let request = RequestEnvelope::new(AuthToken::new("a".repeat(32)).unwrap(), Command::Ping);
        let mut bytes = Vec::new();
        write_message(&mut bytes, &request).unwrap();
        let decoded: RequestEnvelope = read_message(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn oversized_messages_are_rejected_before_decoding() {
        let bytes = vec![b'x'; MAX_MESSAGE_BYTES + 2];
        let result = read_message::<RequestEnvelope>(&mut bytes.as_slice());
        assert!(matches!(
            result,
            Err(TransportError::MessageTooLarge { .. })
        ));
    }
}
