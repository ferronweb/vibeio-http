use super::*;
use crate::h2::codec::Setting;
use crate::h2::error::Reason;
use crate::h2::stream::{BodyMsg, StreamMsg};

/// Runs a connection against a scripted peer over an in-memory
/// duplex stream and collects the server's reply bytes.
///
/// The preface timeout needs the vibeio timer, which does not run
/// under a plain tokio test runtime (same pattern as the h1
/// slowloris test), so a vibeio runtime is built per call.
#[inline]
fn run_connection(
    preface: &[u8],
    frames: &[u8],
    preface_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
) -> Vec<u8> {
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
                let _ = conn
                    .handle(
                        Arc::new(|_| {
                            std::future::pending::<Result<Response<Incoming>, std::io::Error>>()
                        }),
                        ConnectionOptions {
                            idle_timeout,
                            ..Default::default()
                        },
                    )
                    .await;
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

#[inline]
fn decode_frames(wire: &[u8]) -> Vec<Frame> {
    let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
    decoder.extend(wire);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("reply decode") {
        frames.push(frame);
    }
    frames
}

#[inline]
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
        None,
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
        None,
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
        None,
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
        0x00, 0x00, 0x06, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut frames = client_script(|_, _| {});
    frames.extend_from_slice(&bad);
    let reply = run_connection(CLIENT_PREFACE, &frames, Some(Duration::from_secs(5)), None);
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
        None,
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

    let reply = run_connection(CLIENT_PREFACE, &script, Some(Duration::from_secs(5)), None);
    let decoded = decode_frames(&reply);
    // The peer's GOAWAY draws no reply beyond the SETTINGS
    // exchange; the server closes quietly.
    assert!(!decoded.iter().any(|f| matches!(f, Frame::GoAway { .. })));
}

#[test]
fn idle_timeout_closes_connection() {
    // With an idle timeout set, a peer that completes the handshake and
    // then goes silent must be shut down gracefully with a GOAWAY
    // (RFC 9113 Section 10.5).
    let reply = run_connection(
        CLIENT_PREFACE,
        &client_script(|_, _| {}),
        Some(Duration::from_secs(5)),
        Some(Duration::from_millis(30)),
    );
    let decoded = decode_frames(&reply);
    assert!(decoded.iter().any(|f| matches!(f, Frame::GoAway { .. })));
}

#[test]
fn window_update_overflow_is_connection_error() {
    // A WINDOW_UPDATE that pushes the connection window past 2^31-1
    // is a FLOW_CONTROL_ERROR connection error (RFC 9113 6.9.1).
    let (_client, server) = tokio::io::duplex(1 << 16);
    let mut conn = Connection::new(server, Some(Duration::from_secs(5)));
    conn.handle_window_update(0, 0x7fff_ffff);
    let decoded = decode_frames(&conn.out);
    assert!(decoded.iter().any(|f| matches!(
        f,
        Frame::GoAway { error_code, .. } if *error_code == Reason::FlowControlError.code()
    )));
}

#[test]
fn stream_window_update_overflow_is_stream_error() {
    // A WINDOW_UPDATE that overflows a single stream's window is a
    // RST_STREAM with FLOW_CONTROL_ERROR (RFC 9113 6.9.1).
    let (_client, server) = tokio::io::duplex(1 << 16);
    let mut conn = Connection::new(server, Some(Duration::from_secs(5)));
    // Open a stream so it has a flow-control window.
    let (body_tx, _) = kanal::bounded_async::<BodyMsg>(1);
    let (reset_tx, _) = kanal::bounded_async::<u32>(1);
    let (_, msg_rx) = kanal::bounded_async::<StreamMsg>(1);
    conn.streams
        .insert(1, StreamEntry::new(body_tx, reset_tx, msg_rx));
    conn.handle_window_update(1, 0x7fff_ffff);
    let decoded = decode_frames(&conn.out);
    assert!(decoded.iter().any(|f| matches!(
            f,
            Frame::Reset { stream_id, error_code } if *stream_id == 1 && *error_code == Reason::FlowControlError.code()
        )));
}

#[test]
fn graceful_shutdown_queues_goaway() {
    // Cancelling the shutdown token sends GOAWAY (NO_ERROR) and the
    // connection drains in-flight streams before the final GOAWAY.
    vibeio::RuntimeBuilder::new()
        .enable_timer(true)
        .build()
        .unwrap()
        .block_on(async {
            let (_client, server) = tokio::io::duplex(1 << 16);
            let mut conn = Connection::new(server, Some(Duration::from_secs(5)));
            conn.begin_graceful_shutdown();
            assert!(conn.graceful);
            conn.finish_graceful_shutdown();
            let decoded = decode_frames(&conn.out);
            let codes: Vec<u32> = decoded
                .iter()
                .filter_map(|f| match f {
                    Frame::GoAway { error_code, .. } => Some(*error_code),
                    _ => None,
                })
                .collect();
            assert_eq!(codes, vec![Reason::NoError.code(), Reason::NoError.code()]);
        });
}
