//! In-app log ring buffer. Tee tracing's fmt writer into this + stderr.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

const DEFAULT_CAP: usize = 2000;

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    lines: VecDeque<String>,
    cap: usize,
    generation: u64,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::with_capacity(cap.min(DEFAULT_CAP)),
                cap: cap.max(100),
                generation: 0,
            })),
        }
    }

    fn push_line(&self, line: String) {
        if line.is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.lock() {
            if g.lines.len() >= g.cap {
                g.lines.pop_front();
            }
            g.lines.push_back(line);
            g.generation = g.generation.wrapping_add(1);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.lines.clear();
            g.generation = g.generation.wrapping_add(1);
        }
    }

    pub fn snapshot(&self) -> (u64, Vec<String>) {
        let Ok(g) = self.inner.lock() else {
            return (0, Vec::new());
        };
        (g.generation, g.lines.iter().cloned().collect())
    }

    pub fn make_writer(&self) -> BufferMakeWriter {
        BufferMakeWriter {
            buffer: self.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BufferMakeWriter {
    buffer: LogBuffer,
}

impl<'a> MakeWriter<'a> for BufferMakeWriter {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            buffer: self.buffer.clone(),
            pending: String::new(),
        }
    }
}

pub struct BufferWriter {
    buffer: LogBuffer,
    pending: String,
}

impl Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Keep stderr for terminals / `cargo run`; GUI subsystem hides the console window.
        let _ = io::stderr().write_all(buf);

        let text = String::from_utf8_lossy(buf);
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.pending);
                self.buffer
                    .push_line(line.trim_end_matches('\r').to_string());
            } else {
                self.pending.push(ch);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}
