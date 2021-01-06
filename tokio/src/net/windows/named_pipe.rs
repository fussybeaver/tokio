//! Windows named pipes.

use mio::windows::NamedPipe as MioNamedPipe;
use miow::pipe::{NamedPipe as RawNamedPipe, NamedPipeBuilder};
use winapi::{
    shared::winerror::*,
    um::{namedpipeapi::WaitNamedPipeW, winbase::*},
};

use std::{
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    future::Future,
    io::{self, ErrorKind, IoSlice, Result},
    mem,
    os::windows::prelude::*,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

use crate::io::{AsyncRead, AsyncWrite, Interest, PollEvented, ReadBuf, Ready};

const DEFAULT_BUFFER_SIZE: u32 = 65_536;

/// Creates named pipe builder with default parameters.
fn default_builder(addr: &OsStr) -> NamedPipeBuilder {
    let mut builder = NamedPipeBuilder::new(addr);
    builder
        .inbound(true)
        .outbound(true)
        .out_buffer_size(DEFAULT_BUFFER_SIZE)
        .in_buffer_size(DEFAULT_BUFFER_SIZE)
        .max_instances(PIPE_UNLIMITED_INSTANCES as u8);
    builder
}

fn mio_from_miow(pipe: RawNamedPipe) -> MioNamedPipe {
    // Safety: nothing actually unsafe about this. The trait fn includes `unsafe`.
    unsafe { MioNamedPipe::from_raw_handle(pipe.into_raw_handle()) }
}

/// Connecting instance future.
#[derive(Debug)]
enum ConnectingInstance {
    Connecting(NamedPipe),
    Error(io::Error),
    Ready(Option<NamedPipe>),
}

impl ConnectingInstance {
    fn from_mio(mio: MioNamedPipe) -> Result<Self> {
        if connect(&mio)? {
            Ok(Self::Ready(Some(NamedPipe::server(mio)?)))
        } else {
            Ok(Self::Connecting(NamedPipe::server(mio)?))
        }
    }

    /// Creates the first instance of a named pipe server.
    ///
    /// # Errors
    ///
    /// It'll error if pipe with this name already exist.
    fn first(addr: &OsStr) -> Result<Self> {
        let mio = mio_from_miow(default_builder(addr).first(true).create()?);
        Self::from_mio(mio)
    }

    fn _new(addr: &OsStr) -> Result<Self> {
        let mio = mio_from_miow(default_builder(addr).first(false).create()?);
        Self::from_mio(mio)
    }

    /// Creates an instance of a named pipe server.
    ///
    /// In case of an error it will not error immediately,
    /// but will resolve to an error as a future.
    fn new(addr: &OsStr) -> Self {
        match Self::_new(addr) {
            Ok(this) => this,
            Err(err) => Self::Error(err),
        }
    }
}

impl Future for ConnectingInstance {
    type Output = Result<NamedPipe>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match mem::replace(&mut *self, ConnectingInstance::Ready(None)) {
            Self::Ready(None) => {
                // poll on completed future
                Poll::Pending
            }
            Self::Ready(Some(pipe)) => Poll::Ready(Ok(pipe)),
            Self::Connecting(pipe) => match pipe.poll_write_ready(cx) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(pipe)),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => {
                    *self = Self::Connecting(pipe);
                    Poll::Pending
                }
            },
            Self::Error(err) => Poll::Ready(Err(err)),
        }
    }
}

/// Named pipe server.
#[derive(Debug)]
pub struct NamedPipeServer {
    pipe_name: OsString,
    // At least one instance will always exist.
    next_instance: Mutex<ConnectingInstance>,
}

impl NamedPipeServer {
    /// Constructor.
    ///
    /// # Errors
    ///
    /// It'll error if pipe with this name already exists.
    pub fn new<A: Into<OsString>>(addr: A) -> Result<Self> {
        let pipe_name = addr.into();

        // Bind pipe name or fail if already exist to avoid
        // named pipe instance creation race condition.
        let first_instance = ConnectingInstance::first(&pipe_name)?;

        Ok(Self {
            pipe_name,
            next_instance: Mutex::new(first_instance),
        })
    }

    /// Returns `'static` future that will wait for a client.
    pub fn accept(&self) -> impl Future<Output = Result<NamedPipe>> + 'static {
        let next_instance = ConnectingInstance::new(&self.pipe_name);
        mem::replace(&mut *self.next_instance.lock().unwrap(), next_instance)
    }
}

#[derive(Debug)]
enum NamedPipeInner {
    Client(PollEvented<MioNamedPipe>),
    Server(PollEvented<MioNamedPipe>),
}

/// Non-blocking windows named pipe.
#[derive(Debug)]
pub struct NamedPipe {
    inner: NamedPipeInner,
}

impl NamedPipe {
    fn server(mio: MioNamedPipe) -> Result<Self> {
        let io = PollEvented::new(mio)?;
        Ok(Self {
            inner: NamedPipeInner::Server(io),
        })
    }

    fn client(mio: MioNamedPipe) -> Result<Self> {
        let io = PollEvented::new(mio)?;
        Ok(Self {
            inner: NamedPipeInner::Client(io),
        })
    }

    fn io_ref(&self) -> &PollEvented<MioNamedPipe> {
        match &self.inner {
            NamedPipeInner::Client(io) | NamedPipeInner::Server(io) => io,
        }
    }

    /// Will try to connect to a named pipe. Returned pipe may not be writable.
    ///
    /// # Errors
    ///
    /// Will error with `ERROR_PIPE_BUSY` if there are no available instances.
    fn open(addr: &OsStr) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(addr)?;

        let pipe = unsafe { MioNamedPipe::from_raw_handle(file.into_raw_handle()) };
        Self::client(pipe)
    }

    /// Connects to a nemed pipe server by `addr`.
    ///
    /// # Errors
    ///
    /// It'll error if there is no such pipe.
    pub async fn connect<A: AsRef<OsStr>>(addr: A) -> Result<Self> {
        let mut pipe_name = into_wide(addr.as_ref());
        let mut busy = false;

        loop {
            if !busy {
                // pipe instance may be available, so trying to open it
                match Self::open(addr.as_ref()) {
                    Ok(pipe) => {
                        // Pipe is opened.
                        pipe.writable().await?;
                        return Ok(pipe);
                    }
                    Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                        // We should wait since there are no free instances
                    }
                    Err(err) => return Err(err),
                }
            }

            let (status, name) = wait_pipe(pipe_name).await?;
            pipe_name = name;
            busy = matches!(status, WaitPipeResult::Busy);
        }
    }

    /// Polls for read readiness.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.io_ref()
            .registration()
            .poll_read_ready(cx)
            .map_ok(|_| ())
    }

    /// Polls for write readiness.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.io_ref()
            .registration()
            .poll_write_ready(cx)
            .map_ok(|_| ())
    }

    /// Polls for any of the requested ready states.
    pub async fn ready(&self, interest: Interest) -> Result<Ready> {
        let event = self.io_ref().registration().readiness(interest).await?;
        Ok(event.ready)
    }

    /// Waits for the pipe client or server to become readable.
    pub async fn readable(&self) -> Result<()> {
        self.ready(Interest::READABLE).await?;
        Ok(())
    }

    /// Waits for the pipe client or server to become writeable.
    pub async fn writable(&self) -> Result<()> {
        self.ready(Interest::WRITABLE).await?;
        Ok(())
    }
}

impl AsRawHandle for NamedPipe {
    fn as_raw_handle(&self) -> RawHandle {
        self.io_ref().as_raw_handle()
    }
}

impl AsyncRead for NamedPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        unsafe { self.io_ref().poll_read(cx, buf) }
    }
}

impl AsyncWrite for NamedPipe {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.io_ref().poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.io_ref().poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_flush(cx)
    }
}

fn into_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

#[derive(Debug, Clone, Copy)]
enum WaitPipeResult {
    Available,
    Busy,
}

// `wide_name` will be returned.
async fn wait_pipe(wide_name: Vec<u16>) -> Result<(WaitPipeResult, Vec<u16>)> {
    crate::task::spawn_blocking(move || {
        let result = unsafe { WaitNamedPipeW(wide_name.as_ptr(), 0) };
        if result > 0 {
            Ok((WaitPipeResult::Available, wide_name))
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) {
                Ok((WaitPipeResult::Busy, wide_name))
            } else {
                Err(err)
            }
        }
    })
    .await?
}

/// Connects server side of a named pipe.
///
/// Returns `true` if pipe is connected.
fn connect(pipe: &MioNamedPipe) -> io::Result<bool> {
    match pipe.connect() {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
        Err(err) => Err(err),
    }
}
