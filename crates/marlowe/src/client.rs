//! A blocking client for the headless commands: connect, handshake, send, read events.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use marlowe_proto::{write_line, DaemonInfo, Envelope, Event, FrameError, LineReader, Request};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Frame(#[from] FrameError),
    #[error("daemon refused the connection")]
    Refused,
    #[error("daemon closed the connection")]
    Closed,
    #[error("timed out waiting for the daemon")]
    Timeout,
}

pub struct Client {
    writer: TcpStream,
    reader: LineReader<BufReader<TcpStream>>,
}

impl Client {
    /// Connects and completes the token handshake.
    pub fn connect(info: &DaemonInfo, name: &str) -> Result<Self, ClientError> {
        let stream = TcpStream::connect_timeout(&info.addr(), Duration::from_secs(2))?;
        stream.set_nodelay(true).ok();
        let mut writer = stream.try_clone()?;
        let mut reader = LineReader::new(BufReader::new(stream));
        reader.get_ref().get_ref().set_read_timeout(Some(Duration::from_secs(5)))?;
        write_line(
            &mut writer,
            &Request::Hello { token: info.token.clone(), client: name.to_string(), since_seq: None },
        )?;
        let welcome = match reader.next::<Envelope>() {
            Ok(Some(env)) => env.event,
            Ok(None) | Err(_) => return Err(ClientError::Refused),
        };
        if !matches!(welcome, Event::Welcome { .. }) {
            return Err(ClientError::Refused);
        }
        Ok(Self { writer, reader })
    }

    pub fn send(&mut self, req: &Request) -> Result<(), ClientError> {
        write_line(&mut self.writer, req)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Next event; `Ok(None)` when `wait` elapsed with nothing to read.
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<Envelope>, ClientError> {
        self.reader.get_ref().get_ref().set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        match self.reader.next::<Envelope>() {
            Ok(Some(env)) => Ok(Some(env)),
            Ok(None) => Err(ClientError::Closed),
            Err(FrameError::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(e) if e.is_fatal() => Err(e.into()),
            // A line that does not parse is skipped; the stream stays usable.
            Err(_) => Ok(None),
        }
    }

    /// Reads events until `pick` returns a value, `Error` arrives, or `timeout` elapses.
    pub fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut pick: impl FnMut(&Event) -> Option<T>,
    ) -> Result<Result<T, String>, ClientError> {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ClientError::Timeout);
            }
            if let Some(env) = self.next_event(left.min(Duration::from_secs(5)))? {
                if let Some(v) = pick(&env.event) {
                    return Ok(Ok(v));
                }
                if let Event::Error { message, .. } = env.event {
                    return Ok(Err(message));
                }
            }
        }
    }
}
