// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use bytes::Bytes;
use circular_buffer::FixedCircularBuffer;
use futures::StreamExt;
use futures::{Sink, SinkExt, Stream};
use serde::Serialize;
use std::io::{self, ErrorKind, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{
    broadcast,
    mpsc::{self, error::SendError, WeakSender},
    watch, Mutex,
};
use tokio_serial::{DataBits, Parity, SerialPort, SerialPortBuilderExt, StopBits};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_util::codec::{BytesCodec, Decoder};
use tokio_util::sync::PollSender;
use tracing::trace;

use super::console_pty::ConsolePty;

use crate::utils::{string_from_utf16, string_from_utf32};

type RingBuffer = FixedCircularBuffer<u8, OUTPUT_BUF_SIZE>;

const OUTPUT_BUF_SIZE: usize = 16 * 1024;

/// How many chunks of console output the read fan-out holds for a client that
/// is slower than the node. A chunk is one read of the port, and the port is
/// read through `tokio_util`'s `FramedRead`, whose buffer is 8 KiB, so a chunk
/// is at most 8 KiB (a UART read is usually far smaller). The channel keeps a
/// single copy of each chunk however many clients are attached, so the worst
/// case is 256 x 8 KiB = 2 MiB per node, 8 MiB for the four nodes -- and only
/// while a client is not keeping up. At 1.5 Mbaud the line carries about
/// 150 KB/s, so 2 MiB is about 14 s of a saturated line. The earlier 8 chunks
/// were 64 KiB at most: under half a second of it.
pub(crate) const READ_BROADCAST_CAPACITY: usize = 256;

/// A client that falls behind more than [`READ_BROADCAST_CAPACITY`] chunks
/// misses some output. It is told so, in the stream, and stays attached.
fn lag_marker(skipped: u64) -> Bytes {
    let unit = if skipped == 1 { "chunk" } else { "chunks" };
    Bytes::from(format!(
        "\r\n[bmcd: {skipped} {unit} of console output skipped, the browser fell behind]\r\n"
    ))
}

/// The most often a client that keeps falling behind is logged.
const LAG_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Rate limit for the log line about a lagging client: one line per
/// [`LAG_LOG_INTERVAL`], carrying what happened since the last one.
struct LagLog {
    node: usize,
    last: Option<Instant>,
    events: u64,
    chunks: u64,
}

/// What a client reads from the fan-out: a chunk, or, where it fell behind, the
/// marker in place of the chunks it missed. Never an error.
fn lag_to_marker(
    lag: &mut LagLog,
    res: Result<Bytes, BroadcastStreamRecvError>,
) -> io::Result<Bytes> {
    match res {
        Ok(bytes) => Ok(bytes),
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            lag.record(skipped);
            Ok(lag_marker(skipped))
        }
    }
}

impl LagLog {
    fn new(node: usize) -> Self {
        Self {
            node,
            last: None,
            events: 0,
            chunks: 0,
        }
    }

    fn record(&mut self, skipped: u64) {
        if let Some((events, chunks)) = self.note(skipped, Instant::now()) {
            tracing::warn!(
                "[node {}] a console client fell behind: {} chunks skipped in {} events",
                self.node,
                chunks,
                events
            );
        }
    }

    /// Counts a lag event. Returns what is to be logged -- events and chunks
    /// since the last line -- or `None` while the last line is recent.
    fn note(&mut self, skipped: u64, now: Instant) -> Option<(u64, u64)> {
        self.events += 1;
        self.chunks += skipped;
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < LAG_LOG_INTERVAL)
        {
            return None;
        }
        self.last = Some(now);
        Some((
            std::mem::take(&mut self.events),
            std::mem::take(&mut self.chunks),
        ))
    }
}

/// A [`Handler`] is an object that controls a single UART connection from the
/// BMC to a predefined node. It has 2 functions to directly read and write the
/// serial, see [`Self::write`] and [`Self::read_whole_buffer`]. However its
/// recommended to open a channel using the [`Self::open_channel`] method. A
/// [`Handler`] can handle an arbitrary amount of channels which read/write to
/// the UART port.
#[derive(Debug)]
pub struct Handler {
    node: usize,
    /// The speed of the port. The worker follows it: a change is applied to
    /// the open port without reopening it, so the clients attached to the
    /// handler stay attached.
    baud_rate: watch::Sender<u32>,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    path: &'static str,
    ring_buffer: Arc<Mutex<Box<RingBuffer>>>,
    worker_context: Option<(broadcast::Sender<Bytes>, mpsc::Sender<Bytes>)>,
    writer: Option<WeakSender<Bytes>>,
    console: Option<Arc<ConsolePty>>,
}

/// A serial port as the worker sees it: bytes in and out, and a line speed
/// that can be changed while it is open.
pub trait Port: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    fn set_baud(&mut self, baud: u32) -> io::Result<()>;
}

impl Port for tokio_serial::SerialStream {
    fn set_baud(&mut self, baud: u32) -> io::Result<()> {
        self.set_baud_rate(baud).map_err(io::Error::from)
    }
}

impl Handler {
    pub fn new(
        node: usize,
        path: &'static str,
        baud_rate: u32,
        data_bits: DataBits,
        parity: Parity,
        stop_bits: StopBits,
    ) -> Self {
        Handler {
            node,
            path,
            baud_rate: watch::channel(baud_rate).0,
            data_bits,
            parity,
            stop_bits,
            ring_buffer: Arc::new(Mutex::new(RingBuffer::boxed())),
            worker_context: None,
            writer: None,
            console: None,
        }
    }

    /// The speed the port runs at, or will open at.
    pub fn baud_rate(&self) -> u32 {
        *self.baud_rate.borrow()
    }

    /// Sets the speed of the port. A running worker applies it to the open
    /// port at once, without dropping the clients attached; a handler that is
    /// not running opens at it. The value is not checked here, see
    /// [`super::baud::validate_baud`].
    pub fn set_baud_rate(&self, baud: u32) {
        self.baud_rate.send_replace(baud);
    }

    /// Gives the node a shared console device, see
    /// [`super::console_pty`]. Takes effect when [`Self::run`] starts the
    /// worker.
    pub fn set_console(&mut self, console: Option<Arc<ConsolePty>>) {
        self.console = console;
    }

    /// The node's shared console device, if it has one.
    pub fn console(&self) -> Option<&Arc<ConsolePty>> {
        self.console.as_ref()
    }

    /// Returns the current state of the Handler.
    pub fn get_state(&self) -> HandlerState {
        match &self.worker_context {
            Some((_, sender)) if sender.is_closed() => HandlerState::Stopped,
            Some(_) => HandlerState::Running,
            None => HandlerState::Initialized,
        }
    }

    /// Opens a bi-directional asynchronous data-stream which can be used to
    /// read and write bytes from and to the serial port.
    ///
    /// # Returns
    ///
    /// * `SerialError::NotStarted` when [`Self::run`] was not called
    ///   successfully
    pub fn open_channel(
        &self,
    ) -> Result<
        (
            impl Stream<Item = io::Result<Bytes>>,
            impl Sink<bytes::Bytes, Error = io::Error>,
        ),
        SerialError,
    > {
        let Some((read_sender, write_sender)) = &self.worker_context else {
            return Err(SerialError::NotStarted);
        };

        let poll_sender = PollSender::new(write_sender.clone())
            .sink_map_err(|_| io::Error::from(ErrorKind::BrokenPipe));
        // A client that cannot keep up misses output; that is not a reason to
        // end its session. The gap is told to it as a line of text in the
        // stream, and the next chunk follows.
        let mut lag = LagLog::new(self.node);
        let stream = BroadcastStream::new(read_sender.subscribe())
            .map(move |res| lag_to_marker(&mut lag, res));
        Ok((stream, poll_sender))
    }

    /// Reads the whole circular buffer encoded as a String object
    pub async fn read_as_string(&self, encoding: Encoding) -> Result<String, SerialError> {
        let bytes = self.read_whole_buffer().await?;
        let res = match encoding {
            Encoding::Utf8 => String::from_utf8_lossy(&bytes).to_string(),
            Encoding::Utf16 { little_endian } => string_from_utf16(&bytes, little_endian),
            Encoding::Utf32 { little_endian } => string_from_utf32(&bytes, little_endian),
        };
        Ok(res)
    }

    /// This function returns all the cached data.
    /// Time complexity: O(N)
    pub async fn read_whole_buffer(&self) -> Result<Bytes, SerialError> {
        if self.worker_context.is_none() {
            return Err(SerialError::NotStarted);
        };

        let mut rb = self.ring_buffer.lock().await;
        trace!("reading {} from node {}", rb.len(), self.node);
        Ok(Bytes::copy_from_slice(rb.make_contiguous()))
    }

    /// Serial write interface.
    ///
    /// # Returns
    ///
    /// * `SerialError::NotStarted` when [`Self::run`] was not called
    ///   successfully.
    /// * `SerialError::Stopped` when the handler is not running anymore.
    ///
    pub async fn write(&self, bytes: Bytes) -> Result<(), SerialError> {
        let writer = self
            .writer
            .as_ref()
            .ok_or(SerialError::NotStarted)?
            .upgrade()
            .ok_or(SerialError::Stopped)?;

        writer.send(bytes).await?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<(), SerialError> {
        if self.worker_context.take().is_some() {
            return Err(SerialError::AlreadyRunning);
        };

        let mut port = tokio_serial::new(self.path, self.baud_rate())
            .data_bits(self.data_bits)
            .parity(self.parity)
            .stop_bits(self.stop_bits)
            .open_native_async()?;

        // Disable exclusivity of the port to allow other applications to open it.
        // Not a reason to abort if we can't.
        if let Err(e) = port.set_exclusive(false) {
            tracing::warn!("Unable to set exclusivity of port {}: {}", self.path, e);
        }

        self.start(port);
        Ok(())
    }

    /// Starts the worker on an open port. Split from [`Self::run`] so that the
    /// worker can be run against something that is not a UART.
    pub(crate) fn start<P: Port>(&mut self, port: P) {
        self.start_with_capacity(port, READ_BROADCAST_CAPACITY);
    }

    fn start_with_capacity<P: Port>(&mut self, port: P, read_capacity: usize) {
        let (read_sender, _) = broadcast::channel::<Bytes>(read_capacity);
        let (write_sender, write_receiver) = mpsc::channel::<Bytes>(8);
        self.worker_context = Some((read_sender.clone(), write_sender.clone()));

        // What a local tool types on the console device is sent to the node
        // through the same channel the web console uses.
        if let Some(console) = &self.console {
            console.spawn_input(write_sender.downgrade());
        }

        tokio::spawn(run_worker(
            self.node,
            port,
            self.baud_rate.subscribe(),
            self.ring_buffer.clone(),
            read_sender,
            write_receiver,
            self.console.clone(),
        ));

        self.writer = Some(write_sender.downgrade());
    }
}

/// The task that owns the port: what the node sends goes to the ring buffer,
/// the shared console device and every client; what the clients and the
/// console device send goes to the node; a change of the speed is applied to
/// the port.
async fn run_worker<P: Port>(
    node: usize,
    port: P,
    mut baud: watch::Receiver<u32>,
    buffer: Arc<Mutex<Box<RingBuffer>>>,
    read_sender: broadcast::Sender<Bytes>,
    mut write_receiver: mpsc::Receiver<Bytes>,
    console: Option<Arc<ConsolePty>>,
) {
    tracing::info!("[node {}] serial started", &node);
    let mut framed = BytesCodec::new().framed(port);
    loop {
        tokio::select! {
            res = write_receiver.recv() => {
                let Some(data) = res else {
                    tracing::error!("error sending data to uart");
                    break;
                };

                if let Err(e) = framed.send(data).await {
                    tracing::error!("{}", e);
                }
            },
            res = framed.next() => {
                let Some(res) = res else {
                    tracing::error!("Error reading serial stream of node {}", node);
                    break;
                };

                let Ok(bytes) = res else {
                    tracing::error!("Serial stream of node {} has closed", node);
                    break;
                };

                trace!("writing {} bytes node {}", bytes.len(), node);
                // `u8` has no destructor that can fail this write.
                if buffer.lock().await.write(&bytes).is_err() {
                    tracing::error!("Failed to write to buffer of node {}", node);
                    break;
                };

                // Never waits: what the console device cannot take is dropped.
                if let Some(console) = &console {
                    console.publish(&bytes);
                }

                // An error only says that no client is attached, which is no
                // reason to stop reading the node: the last client may leave
                // between the read and this send.
                let _ = read_sender.send(bytes.into());
            },
            res = baud.changed() => {
                if res.is_err() {
                    // The handler is gone.
                    break;
                }
                let speed = *baud.borrow_and_update();
                match framed.get_mut().set_baud(speed) {
                    Ok(()) => tracing::info!("[node {}] serial speed set to {} baud", node, speed),
                    Err(e) => tracing::error!(
                        "[node {}] could not set the serial speed to {} baud: {}",
                        node, speed, e
                    ),
                }
            },
        }
    }
    tracing::warn!("exiting serial worker for node {} ", &node);
}

#[derive(Error, Debug)]
pub enum SerialError {
    #[error("serial worker not started")]
    NotStarted,
    #[error("already running")]
    AlreadyRunning,
    #[error("Stopped")]
    Stopped,
    #[error(transparent)]
    SendError(#[from] SendError<bytes::Bytes>),
    #[error(transparent)]
    TokioSerial(#[from] tokio_serial::Error),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

#[derive(Serialize)]
pub enum HandlerState {
    Initialized,
    Running,
    Stopped,
}

/// Encodings used when reading from a serial port
pub enum Encoding {
    Utf8,
    Utf16 { little_endian: bool },
    Utf32 { little_endian: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_service::console_pty::ConsolePty;
    use std::os::unix::fs::OpenOptionsExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadBuf};

    /// A port with nothing behind it but a pipe: the test plays the node on
    /// the other end, and the speeds the worker sets are written down.
    struct FakePort {
        io: DuplexStream,
        bauds: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl AsyncRead for FakePort {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FakePort {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.io).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    impl Port for FakePort {
        fn set_baud(&mut self, baud: u32) -> io::Result<()> {
            self.bauds.lock().unwrap().push(baud);
            Ok(())
        }
    }

    struct Rig {
        handler: Handler,
        /// The node's end of the line.
        node: DuplexStream,
        bauds: Arc<std::sync::Mutex<Vec<u32>>>,
        link: std::path::PathBuf,
        _dir: TempDir,
    }

    fn rig(with_console: bool, baud: u32) -> Rig {
        rig_with_capacity(with_console, baud, READ_BROADCAST_CAPACITY)
    }

    fn rig_with_capacity(with_console: bool, baud: u32, read_capacity: usize) -> Rig {
        let dir = TempDir::with_prefix("serial_handler").unwrap();
        let link = dir.path().join("ttyNode1");
        let (port_end, node) = tokio::io::duplex(64 * 1024);
        let bauds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handler = Handler::new(
            1,
            "/dev/null",
            baud,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
        );
        if with_console {
            handler.set_console(Some(Arc::new(ConsolePty::create(&link).unwrap())));
        }
        handler.start_with_capacity(
            FakePort {
                io: port_end,
                bauds: bauds.clone(),
            },
            read_capacity,
        );
        Rig {
            handler,
            node,
            bauds,
            link,
            _dir: dir,
        }
    }

    fn open_tool(link: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(link)
            .unwrap()
    }

    async fn eventually(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("condition not reached");
    }

    #[test]
    fn a_handler_opens_at_the_speed_it_was_given() {
        let handler = Handler::new(
            2,
            "/dev/null",
            1_500_000,
            DataBits::Eight,
            Parity::None,
            StopBits::One,
        );
        assert_eq!(handler.baud_rate(), 1_500_000);
        handler.set_baud_rate(9600);
        assert_eq!(handler.baud_rate(), 9600);
    }

    #[tokio::test]
    async fn a_new_speed_reaches_the_open_port_and_keeps_the_clients() {
        let mut rig = rig(false, 115_200);
        let (mut stream, _sink) = rig.handler.open_channel().unwrap();

        rig.handler.set_baud_rate(1_500_000);
        let bauds = rig.bauds.clone();
        eventually(move || *bauds.lock().unwrap() == vec![1_500_000]).await;

        // The client attached before the change still gets what the node sends.
        rig.node.write_all(b"still here").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], b"still here");
    }

    #[tokio::test]
    async fn the_console_device_sees_what_the_web_console_sees() {
        let mut rig = rig(true, 115_200);
        let (mut stream, _sink) = rig.handler.open_channel().unwrap();
        let mut tool = open_tool(&rig.link);

        rig.node.write_all(b"login: ").await.unwrap();
        let web = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&web[..], b"login: ");
        assert_eq!(
            &rig.handler.read_whole_buffer().await.unwrap()[..],
            b"login: "
        );

        let mut seen = Vec::new();
        eventually(|| {
            let mut buf = [0u8; 64];
            if let Ok(n) = std::io::Read::read(&mut tool, &mut buf) {
                seen.extend_from_slice(&buf[..n]);
            }
            seen == b"login: "
        })
        .await;
    }

    #[tokio::test]
    async fn typing_from_the_console_device_or_the_web_reaches_the_node() {
        let mut rig = rig(true, 115_200);
        let mut tool = open_tool(&rig.link);

        std::io::Write::write_all(&mut tool, b"root\r").unwrap();
        rig.handler
            .write(Bytes::from_static(b"pass\r"))
            .await
            .unwrap();

        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 10 {
            let n = tokio::time::timeout(Duration::from_secs(5), rig.node.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        got.sort_unstable();
        let mut expected = b"root\rpass\r".to_vec();
        expected.sort_unstable();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn an_unattended_console_device_does_not_hold_up_the_serial_side() {
        let mut rig = rig(true, 115_200);

        // Nobody has the console device open, and the node says far more
        // than the pty will ever hold.
        let chunk = vec![b'.'; 4096];
        tokio::time::timeout(Duration::from_secs(30), async {
            for _ in 0..(2 * 1024 * 1024) / chunk.len() {
                rig.node.write_all(&chunk).await.unwrap();
            }
            rig.node.write_all(b"END").await.unwrap();
        })
        .await
        .expect("the serial side stalled on the console device");

        // Everything was read off the line and kept for the clients.
        let mut tail = Bytes::new();
        for _ in 0..200 {
            tail = rig.handler.read_whole_buffer().await.unwrap();
            if tail.ends_with(b"END") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(tail.ends_with(b"END"));

        // And the line to the node still works.
        rig.handler.write(Bytes::from_static(b"x")).await.unwrap();
        let mut b = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(5), rig.node.read_exact(&mut b))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&b, b"x");
    }

    /// The node says `n` things, one chunk each: every one is read off the
    /// line and into the ring buffer before the next is sent.
    async fn say_in_chunks(rig: &mut Rig, n: usize) {
        for i in 0..n {
            rig.node.write_all(&[b'a' + (i % 26) as u8]).await.unwrap();
            for _ in 0..2000 {
                if rig.handler.read_whole_buffer().await.unwrap().len() == i + 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(rig.handler.read_whole_buffer().await.unwrap().len(), i + 1);
        }
    }

    async fn next_chunk(stream: &mut (impl Stream<Item = io::Result<Bytes>> + Unpin)) -> Bytes {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("no chunk")
            .expect("the stream ended")
            .expect("the stream failed")
    }

    #[test]
    fn the_read_fan_out_holds_a_burst_of_a_fast_line() {
        // 2 MiB at most per node, see the constant. Going back to a handful
        // of chunks makes a line at 1.5 Mbaud overrun a browser at once.
        const { assert!(READ_BROADCAST_CAPACITY >= 256) };
    }

    #[tokio::test]
    async fn a_client_that_reads_late_gets_a_burst_of_chunks_whole() {
        let mut rig = rig(false, 1_500_000);
        let (mut stream, _sink) = rig.handler.open_channel().unwrap();

        // Far more than the 8 chunks the channel once held, none read yet.
        say_in_chunks(&mut rig, 200).await;

        for i in 0..200 {
            let chunk = next_chunk(&mut stream).await;
            assert_eq!(&chunk[..], &[b'a' + (i % 26) as u8], "chunk {i}");
        }
    }

    #[tokio::test]
    async fn a_client_that_fell_behind_is_told_and_kept() {
        let mut rig = rig_with_capacity(false, 115_200, 2);
        let (mut slow, _s1) = rig.handler.open_channel().unwrap();
        let (mut quick, _s2) = rig.handler.open_channel().unwrap();

        // Five chunks into a channel of two while `slow` reads nothing: the
        // first three are gone. `quick` keeps pace, which also tells us each
        // chunk has been sent before the next is written.
        for i in 0..5u8 {
            rig.node.write_all(&[b'a' + i]).await.unwrap();
            assert_eq!(&next_chunk(&mut quick).await[..], &[b'a' + i]);
        }

        let marker = next_chunk(&mut slow).await;
        assert_eq!(
            &marker[..],
            b"\r\n[bmcd: 3 chunks of console output skipped, the browser fell behind]\r\n"
        );
        // The session goes on with what was kept, and with what comes next.
        assert_eq!(&next_chunk(&mut slow).await[..], b"d");
        assert_eq!(&next_chunk(&mut slow).await[..], b"e");
        rig.node.write_all(b"after").await.unwrap();
        assert_eq!(&next_chunk(&mut slow).await[..], b"after");
    }

    #[test]
    fn the_marker_names_what_was_skipped() {
        assert_eq!(
            &lag_marker(1)[..],
            b"\r\n[bmcd: 1 chunk of console output skipped, the browser fell behind]\r\n"
        );
        assert!(lag_marker(250).starts_with(b"\r\n[bmcd: 250 chunks of"));
    }

    #[test]
    fn a_client_that_keeps_lagging_is_logged_once_in_a_while() {
        let mut log = LagLog::new(1);
        let t0 = Instant::now();
        assert_eq!(log.note(3, t0), Some((1, 3)));
        assert_eq!(log.note(4, t0 + Duration::from_secs(1)), None);
        assert_eq!(log.note(5, t0 + Duration::from_secs(9)), None);
        // What was held back is reported with the next line.
        assert_eq!(log.note(1, t0 + Duration::from_secs(10)), Some((3, 10)));
        assert_eq!(log.note(2, t0 + Duration::from_secs(11)), None);
    }
}
