use std::io::Write;
use std::sync::{Arc, Mutex};

use tst_core::transport::{Transport, TransportError};

/// Collects every byte the tap sends, flattened.
pub struct VecTransport(pub Arc<Mutex<Vec<u8>>>);

impl Transport for VecTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.0
            .lock()
            .expect("VecTransport mutex")
            .extend_from_slice(msg);
        Ok(())
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn close(&mut self) {}
}

/// Collects the tap's JSONL log.
pub struct VecWriter(pub Arc<Mutex<Vec<u8>>>);

impl Write for VecWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("VecWriter mutex").extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
