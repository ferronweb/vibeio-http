//! HTTP/2 connection skeleton (RFC 9113 Sections 3.5 and 6.5).
//!
//! Drives one HTTP/2 connection over an async I/O stream: reads the
//! client's 24-octet preface (with a timeout), sends the server
//! SETTINGS, maintains the peer's SETTINGS state (applying
//! `SETTINGS_HEADER_TABLE_SIZE` to the HPACK encoder and
//! `SETTINGS_MAX_FRAME_SIZE` to the outgoing frame writer), answers
//! SETTINGS frames with ACK, echoes PING, and reports protocol
//! violations with GOAWAY before closing.
//!
//! Frame parsing and validation happen in [`super::codec`]; this module
//! adds the connection-level wiring. DATA/HEADERS/PRIORITY/RST_STREAM/
//! WINDOW_UPDATE/CONTINUATION/PUSH_PROMISE frames are parsed and
//! validated but not acted on yet (stream handling lands in C2).

use std::time::Duration;

use super::codec::{
    Frame, FrameDecoder, FrameWriter, CLIENT_PREFACE, DEFAULT_INITIAL_WINDOW_SIZE,
    DEFAULT_MAX_FRAME_SIZE,
};
use super::error::Reason;
use super::hpack::Encoder;

/// The peer's current SETTINGS values. Defaults are the RFC 9113
/// Section 6.5.2 initial values; they change as non-ACK SETTINGS frames
/// arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerSettings {
    pub(crate) header_table_size: u32,
    pub(crate) enable_push: u32,
    pub(crate) initial_window_size: u32,
    pub(crate) max_frame_size: usize,
    /// Kept for the field block decoder's bomb protection (C2).
    #[allow(dead_code)]
    pub(crate) max_header_list_size: u32,
}

impl Default for PeerSettings {
    fn default() -> Self {
        PeerSettings {
            header_table_size: 4096,
            enable_push: 1,
            initial_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_header_list_size: u32::MAX,
        }
    }
}

/// An HTTP/2 server connection.
///
/// `Io` must be a raw transport: the preface is read byte-exact, so
/// any buffering layer between the socket and this type breaks the
/// initial read.
pub struct Connection<Io> {
    io: Io,
    decoder: FrameDecoder,
    writer: FrameWriter,
    out: Vec<u8>,
    /// Encoder for the field blocks this connection sends; its table
    /// size follows the peer's `SETTINGS_HEADER_TABLE_SIZE`.
    encoder: Encoder,
    /// The peer's settings, updated by non-ACK SETTINGS frames.
    peer: PeerSettings,
    /// The settings this connection announced (an empty SETTINGS frame:
    /// all RFC defaults).
    #[allow(dead_code)]
    local: PeerSettings,
    /// Bounds the wait for the client's 24-octet preface. A peer that
    /// is too slow is disconnected without a GOAWAY.
    preface_timeout: Option<Duration>,
}

impl<Io> Connection<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    /// Creates a connection over `io`.
    pub fn new(io: Io, preface_timeout: Option<Duration>) -> Connection<Io> {
        Connection {
            io,
            decoder: FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE),
            writer: FrameWriter::new(DEFAULT_MAX_FRAME_SIZE),
            out: Vec::new(),
            encoder: Encoder::new(4096),
            peer: PeerSettings::default(),
            local: PeerSettings::default(),
            preface_timeout,
        }
    }

    /// Drives the connection to completion: peer EOF, GOAWAY received,
    /// preface timeout, or an unrecoverable protocol error.
    pub async fn drive(mut self) -> std::io::Result<()> {
        match self.read_preface().await? {
            None => return Ok(()), // preface timeout: close quietly
            Some(false) => {
                // Invalid preface: connection error PROTOCOL_ERROR
                // (RFC 9113 Section 3.5), then close.
                self.goaway(Reason::ProtocolError, b"invalid connection preface");
                self.flush().await?;
                return Ok(());
            }
            Some(true) => {}
        }

        // Our connection preface: the SETTINGS frame (RFC 9113
        // Section 3.5). All RFC defaults are announced, so the payload
        // is empty.
        self.writer.write_settings(&mut self.out, &[]);
        self.flush().await?;

        let mut buf = [0u8; 8192];
        let mut peer_goaway = false;
        while !peer_goaway {
            let n = tokio::io::AsyncReadExt::read(&mut self.io, &mut buf).await?;
            if n == 0 {
                break; // peer closed; nothing more to say
            }
            self.decoder.extend(&buf[..n]);
            peer_goaway = self.process_frames().await?;
        }
        self.flush().await?;
        Ok(())
    }

    /// Reads the 24-octet client preface.
    ///
    /// Returns `Ok(None)` on timeout (the connection closes quietly —
    /// answering a peer that never spoke is meaningless), `Ok(Some(
    /// true))` on a match, and `Ok(Some(false))` when the peer sent
    /// something else (the caller answers with GOAWAY).
    async fn read_preface(&mut self) -> std::io::Result<Option<bool>> {
        let mut magic = [0u8; CLIENT_PREFACE.len()];
        match self.preface_timeout {
            Some(timeout) => {
                match vibeio::time::timeout(
                    timeout,
                    tokio::io::AsyncReadExt::read_exact(&mut self.io, &mut magic),
                )
                .await
                {
                    Ok(result) => {
                        result?;
                    }
                    Err(_elapsed) => return Ok(None),
                }
            }
            None => {
                tokio::io::AsyncReadExt::read_exact(&mut self.io, &mut magic).await?;
            }
        }
        Ok(Some(magic == CLIENT_PREFACE))
    }

    /// Decodes and handles every frame currently buffered. Returns
    /// `Ok(true)` when the connection should end (peer GOAWAY or an
    /// error we answered with GOAWAY).
    async fn process_frames(&mut self) -> std::io::Result<bool> {
        loop {
            let frame = match self.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(false),
                Err(error) => {
                    // Frame-level violation: GOAWAY with the code the
                    // codec determined (RFC 9113 Sections 6.10, 6.5.2),
                    // then close.
                    self.goaway(error.reason, b"frame error");
                    self.flush().await?;
                    return Ok(true);
                }
            };
            match frame {
                Frame::Settings {
                    ack: false,
                    settings,
                } => {
                    self.apply_peer_settings(&settings);
                    self.writer.write_settings_ack(&mut self.out);
                }
                Frame::Settings { ack: true, .. } => {}
                Frame::Ping {
                    ack: false,
                    payload,
                } => {
                    self.writer.write_ping_ack(&mut self.out, &payload);
                }
                Frame::Ping { ack: true, .. } => {}
                Frame::GoAway { .. } => return Ok(true),
                // Stream-level frames are parsed and validated by the
                // codec already; acting on them lands with the stream
                // state machine (C2).
                _ => {}
            }
            if !self.out.is_empty() {
                self.flush().await?;
            }
        }
    }

    /// Applies a non-ACK SETTINGS payload to the peer state and our
    /// send-side configuration. Validation happened in the codec, so
    /// every value here is in range for its identifier.
    fn apply_peer_settings(&mut self, settings: &[super::codec::Setting]) {
        for setting in settings {
            match setting.id {
                0x01 => {
                    // SETTINGS_HEADER_TABLE_SIZE: the peer's decode
                    // table, i.e. our encode table (RFC 9113
                    // Section 6.5.2, RFC 7541 Section 4.2).
                    self.peer.header_table_size = setting.value;
                    self.encoder.queue_size_update(setting.value as usize);
                }
                0x02 => self.peer.enable_push = setting.value,
                0x04 => self.peer.initial_window_size = setting.value,
                0x05 => {
                    // SETTINGS_MAX_FRAME_SIZE: cap for frames we send.
                    self.peer.max_frame_size = setting.value as usize;
                    self.writer.max_frame_size = setting.value as usize;
                }
                0x06 => self.peer.max_header_list_size = setting.value,
                _ => {}
            }
        }
    }

    /// Queues a GOAWAY frame; the connection closes after it flushes.
    fn goaway(&mut self, reason: Reason, debug: &[u8]) {
        self.writer
            .write_goaway(&mut self.out, 0, reason.code(), debug);
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        if !self.out.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut self.io, &self.out).await?;
            self.out.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::codec::Setting;

    /// Runs a connection against a scripted peer over an in-memory
    /// duplex stream and collects the server's reply bytes.
    ///
    /// The preface timeout needs the vibeio timer, which does not run
    /// under a plain tokio test runtime (same pattern as the h1
    /// slowloris test), so a vibeio runtime is built per call.
    fn run_connection(preface: &[u8], frames: &[u8], preface_timeout: Option<Duration>) -> Vec<u8> {
        let script: Vec<u8> = [preface, frames].concat();
        vibeio::RuntimeBuilder::new()
            .enable_timer(true)
            .build()
            .unwrap()
            .block_on(async move {
                let (client_end, server_end) = tokio::io::duplex(1 << 16);
                let script = script;

                let server = vibeio::spawn(async move {
                    let conn = Connection::new(server_end, preface_timeout);
                    let _ = conn.drive().await;
                });

                let mut client = client_end;
                tokio::io::AsyncWriteExt::write_all(&mut client, &script)
                    .await
                    .expect("write script");
                vibeio::time::sleep(Duration::from_millis(50)).await;

                let mut reply = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    // The server answers in one burst and then waits for
                    // our EOF; give up after a short idle gap instead of
                    // blocking on the half-open duplex.
                    let read = vibeio::time::timeout(
                        Duration::from_millis(100),
                        tokio::io::AsyncReadExt::read(&mut client, &mut buf),
                    )
                    .await;
                    match read {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => reply.extend_from_slice(&buf[..n]),
                    }
                }
                // Close our half so the server sees EOF and exits.
                drop(client);
                vibeio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .expect("server did not finish");
                reply
            })
    }

    fn decode_frames(wire: &[u8]) -> Vec<Frame> {
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.extend(wire);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("reply decode") {
            frames.push(frame);
        }
        frames
    }

    fn client_script(writer: impl FnOnce(&mut FrameWriter, &mut Vec<u8>)) -> Vec<u8> {
        let mut script = Vec::new();
        FrameWriter::new(DEFAULT_MAX_FRAME_SIZE).write_settings(&mut script, &[]);
        writer(&mut FrameWriter::new(DEFAULT_MAX_FRAME_SIZE), &mut script);
        script
    }

    #[test]
    fn valid_preface_completes_handshake() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|_, _| {}),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);

        // The server's connection preface: its own SETTINGS.
        assert!(matches!(
            decoded.first(),
            Some(Frame::Settings { ack: false, .. })
        ));
        // The client's SETTINGS is acknowledged.
        assert!(decoded
            .iter()
            .any(|f| matches!(f, Frame::Settings { ack: true, .. })));
    }

    #[test]
    fn invalid_preface_answers_goaway_protocol_error() {
        let reply = run_connection(
            b"INVALID CONNECTION PREFACE!!",
            &[],
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        assert_eq!(decoded.len(), 1);
        assert!(matches!(
            &decoded[0],
            Frame::GoAway {
                error_code: 0x01,
                ..
            }
        ));
    }

    #[test]
    fn settings_are_applied_and_acked() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|writer, script| {
                writer.write_settings(
                    script,
                    &[
                        Setting {
                            id: 0x01,
                            value: 16_384,
                        },
                        Setting { id: 0x02, value: 0 },
                        Setting {
                            id: 0x04,
                            value: 1_048_576,
                        },
                        Setting {
                            id: 0x05,
                            value: 32_768,
                        },
                        Setting {
                            id: 0x06,
                            value: 65_536,
                        },
                        Setting { id: 0x63, value: 7 }, // unknown: ignored
                    ],
                );
            }),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        // One ACK per non-ACK SETTINGS frame, in order.
        let acks: Vec<&Frame> = decoded
            .iter()
            .filter(|f| matches!(f, Frame::Settings { ack: true, .. }))
            .collect();
        assert_eq!(acks.len(), 2);
    }

    #[test]
    fn settings_ack_with_payload_is_connection_error() {
        // SETTINGS with the ACK flag and a non-empty payload: the codec
        // rejects it and the connection reports FRAME_SIZE_ERROR.
        // Payload: one Setting { id: 0x04, value: 0 }.
        let bad = [
            0x00, 0x00, 0x06, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x00,
        ];
        let mut frames = client_script(|_, _| {});
        frames.extend_from_slice(&bad);
        let reply = run_connection(CLIENT_PREFACE, &frames, Some(Duration::from_secs(5)));
        let decoded = decode_frames(&reply);
        assert!(matches!(
            decoded.last().and_then(|f| match f {
                Frame::GoAway { error_code, .. } => Some(*error_code),
                _ => None,
            }),
            Some(0x06) // FRAME_SIZE_ERROR
        ));
    }

    #[test]
    fn ping_is_echoed() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|writer, script| {
                writer.write_ping(script, &[1, 2, 3, 4, 5, 6, 7, 8]);
            }),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        assert!(decoded.iter().any(|f| matches!(
            f,
            Frame::Ping { ack: true, payload } if *payload == [1, 2, 3, 4, 5, 6, 7, 8]
        )));
    }

    #[test]
    fn goaway_from_peer_ends_connection() {
        let mut script = client_script(|_, _| {});
        FrameWriter::new(DEFAULT_MAX_FRAME_SIZE).write_goaway(&mut script, 0, 0x00, b"bye");

        let reply = run_connection(CLIENT_PREFACE, &script, Some(Duration::from_secs(5)));
        let decoded = decode_frames(&reply);
        // The peer's GOAWAY draws no reply beyond the SETTINGS
        // exchange; the server closes quietly.
        assert!(!decoded.iter().any(|f| matches!(f, Frame::GoAway { .. })));
    }
}
