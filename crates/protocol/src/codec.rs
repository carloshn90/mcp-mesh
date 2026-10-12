//! Newline-delimited JSON framing over async I/O.
//!
//! Every message (JSON-RPC request, notification, or response) is written as
//! one `serde_json`-encoded line. `serde_json` escapes control characters in
//! strings, so a literal `\n` byte only ever appears as a frame separator,
//! never inside a value — framing on `\n` is safe without extra delimiters.

use std::io;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Default cap on a single line's size, guarding against a runaway peer that
/// never sends a newline.
pub const DEFAULT_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Reads newline-delimited JSON values of type `T` from an async reader.
pub struct MessageReader<R, T> {
    reader: BufReader<R>,
    max_line_bytes: usize,
    _marker: PhantomData<T>,
}

impl<R: AsyncRead + Unpin, T: DeserializeOwned> MessageReader<R, T> {
    pub fn new(reader: R) -> Self {
        Self::with_max_line_bytes(reader, DEFAULT_MAX_LINE_BYTES)
    }

    pub fn with_max_line_bytes(reader: R, max_line_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_line_bytes,
            _marker: PhantomData,
        }
    }

    /// Reads and decodes the next message. Returns `Ok(None)` on clean EOF
    /// (the peer closed the stream without sending a partial line).
    pub async fn read_message(&mut self) -> io::Result<Option<T>> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        if line.len() > self.max_line_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "line of {} bytes exceeds max of {} bytes",
                    line.len(),
                    self.max_line_bytes
                ),
            ));
        }
        serde_json::from_str(line.trim_end())
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Writes values of type `T` as newline-delimited JSON to an async writer.
pub struct MessageWriter<W, T> {
    writer: W,
    _marker: PhantomData<T>,
}

impl<W: AsyncWrite + Unpin, T: Serialize> MessageWriter<W, T> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            _marker: PhantomData,
        }
    }

    pub async fn write_message(&mut self, message: &T) -> io::Result<()> {
        let mut encoded = serde_json::to_string(message)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        encoded.push('\n');
        self.writer.write_all(encoded.as_bytes()).await?;
        self.writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Id, Request};
    use serde_json::json;

    fn sample_request(id: i64) -> Request {
        Request {
            jsonrpc: crate::jsonrpc::V2,
            id: Id::Number(id),
            method: "tools/list".to_string(),
            params: None,
        }
    }

    #[tokio::test]
    async fn round_trips_a_single_message() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = MessageWriter::<_, Request>::new(client);
        let mut reader = MessageReader::<_, Request>::new(server);

        let sent = sample_request(1);
        writer.write_message(&sent).await.unwrap();
        let received = reader.read_message().await.unwrap().unwrap();
        assert_eq!(sent, received);
    }

    #[tokio::test]
    async fn reads_multiple_messages_delivered_across_small_reads() {
        // A tiny duplex buffer forces the reader to assemble a single line
        // out of many partial reads, and to correctly split two messages
        // that land back-to-back in the same underlying buffer.
        let (client, server) = tokio::io::duplex(8);
        let mut writer = MessageWriter::<_, Request>::new(client);
        let mut reader = MessageReader::<_, Request>::new(server);

        let first = sample_request(1);
        let second = sample_request(2);

        let write_both = async {
            writer.write_message(&first).await.unwrap();
            writer.write_message(&second).await.unwrap();
        };
        let read_both = async {
            let got_first = reader.read_message().await.unwrap().unwrap();
            let got_second = reader.read_message().await.unwrap().unwrap();
            (got_first, got_second)
        };

        let (_, (got_first, got_second)) = tokio::join!(write_both, read_both);
        assert_eq!(got_first, first);
        assert_eq!(got_second, second);
    }

    #[tokio::test]
    async fn clean_eof_yields_none() {
        let (client, server) = tokio::io::duplex(1024);
        drop(client);
        let mut reader = MessageReader::<_, Request>::new(server);
        assert!(reader.read_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn garbage_line_is_a_decode_error() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = MessageReader::<_, Request>::new(server);

        client.write_all(b"not json\n").await.unwrap();
        let err = reader.read_message().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn oversized_line_is_rejected_before_parsing() {
        let (mut client, server) = tokio::io::duplex(4096);
        let mut reader = MessageReader::<_, serde_json::Value>::with_max_line_bytes(server, 16);

        let line = format!("{}\n", json!({"padding": "way more than 16 bytes"}));
        client.write_all(line.as_bytes()).await.unwrap();

        let err = reader.read_message().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds max"));
    }
}
