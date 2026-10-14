//! Reads arbitrary bytes the way the service reads a client connection.
//! Beyond not crashing, a header over its kind's limit must be refused
//! before its body is needed, and every message that decodes must decode
//! the same way after encoding it again.

#![no_main]

use std::sync::OnceLock;

use hyakume::ipc::{self, ClientMessage, Frame, IpcError};
use libfuzzer_sys::fuzz_target;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    })
}

fuzz_target!(|data: &[u8]| {
    runtime().block_on(async {
        if let Some(header) = data.first_chunk::<5>() {
            let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            if len > ipc::MAX_FRAME_BYTES {
                assert!(matches!(
                    ipc::FrameReader::new(&header[..]).next().await,
                    Err(IpcError::TooLarge | IpcError::Malformed)
                ));
            }
        }
        let mut frames = ipc::FrameReader::new(data);
        while let Ok(frame) = frames.next().await {
            if let Frame::Message(body) = frame
                && let Ok(msg) = ipc::decode::<ClientMessage>(&body)
            {
                let again = serde_json::to_vec(&msg).unwrap();
                let back: ClientMessage = ipc::decode(&again).unwrap();
                assert_eq!(format!("{msg:?}"), format!("{back:?}"));
            }
        }
    });
});
