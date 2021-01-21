//! Main module of rexpect: start new process and interact with it

use crate::errors::*; // load error-chain
pub use crate::reader::ReadUntil;
use crate::reader::Regex;
use std::time;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_stream::StreamExt;

pub struct StreamSession<
    R: tokio_stream::Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> + Unpin,
    W: AsyncWrite + Unpin,
> {
    reader: R,
    writer: W,
    buffer: Vec<u8>,
    eof: bool,
    timeout: Option<time::Duration>,
}

impl<
        R: tokio_stream::Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> + Unpin,
        W: AsyncWrite + Unpin,
    > StreamSession<R, W>
{
    pub async fn new(reader: R, writer: W, timeout: Option<u64>) -> Self {
        Self {
            reader: reader.into(),
            writer,
            buffer: Vec::with_capacity(1024),
            eof: false,
            timeout: timeout.and_then(|millis| Some(time::Duration::from_millis(millis))),
        }
    }

    /// sends string and a newline to process
    ///
    /// this is guaranteed to be flushed to the process
    /// returns number of written bytes
    pub async fn send_line(&mut self, line: &str) -> Result<usize> {
        let mut len = self.send(line).await?;
        len += self
            .writer
            .write(&['\n' as u8])
            .await
            .chain_err(|| "cannot write newline")?;
        Ok(len)
    }

    /// Send string to process. As stdin of the process is most likely buffered, you'd
    /// need to call `flush()` after `send()` to make the process actually see your input.
    ///
    /// Returns number of written bytes
    pub async fn send(&mut self, s: &str) -> Result<usize> {
        self.writer
            .write(s.as_bytes())
            .await
            .chain_err(|| "cannot write line to process")
    }

    pub async fn send_bytes(&mut self, buf: &[u8]) -> Result<usize> {
        self.writer
            .write(buf)
            .await
            .chain_err(|| "cannot write bytes to process")
    }

    /// Send a control code to the running process and consume resulting output line
    /// (which is empty because echo is off)
    ///
    /// E.g. `send_control('c')` sends ctrl-c. Upper/smaller case does not matter.
    pub async fn send_control(&mut self, c: char) -> Result<()> {
        let code = match c {
            'a'..='z' => c as u8 + 1 - 'a' as u8,
            'A'..='Z' => c as u8 + 1 - 'A' as u8,
            '[' => 27,
            '\\' => 28,
            ']' => 29,
            '^' => 30,
            '_' => 31,
            _ => return Err(format!("I don't understand Ctrl-{}", c).into()),
        };
        self.writer
            .write_all(&[code])
            .await
            .chain_err(|| "cannot send control")?;
        // stdout is line buffered, so needs a flush
        self.writer
            .flush()
            .await
            .chain_err(|| "cannot flush after sending ctrl keycode")?;
        Ok(())
    }

    /// Make sure all bytes written via `send()` are sent to the process
    pub async fn flush(&mut self) -> Result<()> {
        self.writer.flush().await.chain_err(|| "could not flush")
    }

    /// reads all available chars from the read channel and stores them in self.buffer
    async fn read_into_buffer(&mut self) -> Result<()> {
        if self.eof {
            return Ok(());
        }
        match self.reader.next().await {
            Some(Ok(c)) => self.buffer.append(&mut c.to_vec()),
            Some(Err(x)) if x.kind() == std::io::ErrorKind::UnexpectedEof => self.eof = true,
            // discard other errors
            Some(Err(_)) => {}
            None => self.eof = true,
        }
        Ok(())
    }

    /// Read until needle is found (blocking!) and return tuple with:
    /// 1. yet unread string until and without needle
    /// 2. matched needle
    ///
    /// This methods loops (while reading from the Cursor) until the needle is found.
    ///
    /// There are different modes:
    ///
    /// - `ReadUntil::String` searches for string (use '\n'.to_string() to search for newline).
    ///   Returns not yet read data in first String, and needle in second String
    /// - `ReadUntil::Regex` searches for regex
    ///   Returns not yet read data in first String and matched regex in second String
    /// - `ReadUntil::NBytes` reads maximum n bytes
    ///   Returns n bytes in second String, first String is left empty
    /// - `ReadUntil::EOF` reads until end of file is reached
    ///   Returns all bytes in second String, first is left empty
    ///
    /// Note that when used with a tty the lines end with \r\n
    ///
    /// Returns error if EOF is reached before the needle could be found.
    ///
    /// # Example with line reading, byte reading, regex and EOF reading.
    ///
    /// ```
    /// # use std::io::Cursor;
    /// use rexpect::reader::{NBReader, ReadUntil, Regex};
    /// // instead of a Cursor you would put your process output or file here
    /// let f = Cursor::new("Hello, miss!\n\
    ///                         What do you mean: 'miss'?");
    /// let mut e = NBReader::new(f, None);
    ///
    /// let (first_line, _) = e.read_until(&ReadUntil::String('\n'.to_string())).unwrap();
    /// assert_eq!("Hello, miss!", &first_line);
    ///
    /// let (_, two_bytes) = e.read_until(&ReadUntil::NBytes(2)).unwrap();
    /// assert_eq!("Wh", &two_bytes);
    ///
    /// let re = Regex::new(r"'[a-z]+'").unwrap(); // will find 'miss'
    /// let (before, reg_match) = e.read_until(&ReadUntil::Regex(re)).unwrap();
    /// assert_eq!("at do you mean: ", &before);
    /// assert_eq!("'miss'", &reg_match);
    ///
    /// let (_, until_end) = e.read_until(&ReadUntil::EOF).unwrap();
    /// assert_eq!("?", &until_end);
    /// ```
    ///
    pub async fn read_bytes_until(&mut self, needle: &ReadUntil) -> Result<(Vec<u8>, Vec<u8>)> {
        let start = time::Instant::now();

        loop {
            self.read_into_buffer().await?;
            if let Some(tuple_pos) = crate::reader::find(needle, &self.buffer, self.eof) {
                let first = self.buffer.drain(..tuple_pos.0).collect();
                let second = self.buffer.drain(..tuple_pos.1 - tuple_pos.0).collect();
                return Ok((first, second));
            }

            // reached end of stream and didn't match -> error
            // we don't know the reason of eof yet, so we provide an empty string
            // this will be filled out in session::exp()
            if self.eof {
                return Err(ErrorKind::EOF(needle.to_string(), self.buffer.clone(), None).into());
            }

            // ran into timeout
            if let Some(timeout) = self.timeout {
                if start.elapsed() > timeout {
                    return Err(ErrorKind::Timeout(
                        needle.to_string(),
                        match std::str::from_utf8(self.buffer.as_slice()) {
                            Ok(x) => format!("\"{}\"", x)
                                .replace("\n", "`\\n`\n")
                                .replace("\r", "`\\r`")
                                .replace('\u{1b}', "`^`"),
                            Err(_) => format!("{:x?}", self.buffer),
                        },
                        timeout,
                    )
                    .into());
                }
            }
        }
    }

    pub async fn read_until(&mut self, needle: &ReadUntil) -> Result<(String, String)> {
        self.read_bytes_until(needle).await.map(|x| {
            (
                match std::str::from_utf8(&x.0) {
                    Ok(x) => x.to_string(),
                    Err(_) => format!("{:x?}", x.0),
                },
                match std::str::from_utf8(&x.1) {
                    Ok(x) => x.to_string(),
                    Err(_) => format!("{:x?}", x.1),
                },
            )
        })
    }

    // wrapper around reader::read_until to give more context for errors
    async fn exp(&mut self, needle: &ReadUntil) -> Result<(String, String)> {
        self.read_until(needle).await
    }

    /// Wait until we see EOF (i.e. child process has terminated)
    /// Return all the yet unread output
    pub async fn exp_eof(&mut self) -> Result<String> {
        self.exp(&ReadUntil::EOF).await.and_then(|(_, s)| Ok(s))
    }

    /// Wait until provided regex is seen on stdout of child process.
    /// Return a tuple:
    /// 1. the yet unread output
    /// 2. the matched regex
    ///
    /// Note that `exp_regex("^foo")` matches the start of the yet consumed output.
    /// For matching the start of the line use `exp_regex("\nfoo")`
    pub async fn exp_regex(&mut self, regex: &str) -> Result<(String, String)> {
        let res = self
            .exp(&ReadUntil::Regex(
                Regex::new(regex).chain_err(|| "invalid regex")?,
            ))
            .await
            .and_then(|s| Ok(s));
        res
    }

    /// Wait until provided string is seen on stdout of child process.
    /// Return the yet unread output (without the matched string)
    pub async fn exp_string(&mut self, needle: &str) -> Result<String> {
        self.exp(&ReadUntil::String(needle.to_string()))
            .await
            .and_then(|(s, _)| Ok(s))
    }

    pub async fn exp_bytes(&mut self, needle: Vec<u8>) -> Result<Vec<u8>> {
        self.read_bytes_until(&ReadUntil::Bytes(needle))
            .await
            .and_then(|(s, _)| Ok(s))
    }

    /// Wait until provided char is seen on stdout of child process.
    /// Return the yet unread output (without the matched char)
    pub async fn exp_char(&mut self, needle: char) -> Result<String> {
        self.exp(&ReadUntil::String(needle.to_string()))
            .await
            .and_then(|(s, _)| Ok(s))
    }

    /// Wait until any of the provided needles is found.
    ///
    /// Return a tuple with:
    /// 1. the yet unread string, without the matching needle (empty in case of EOF and NBytes)
    /// 2. the matched string
    ///
    /// # Example:
    ///
    /// ```
    /// use rexpect::{spawn, ReadUntil};
    /// # use rexpect::errors::*;
    ///
    /// # fn main() {
    ///     # || -> Result<()> {
    /// let mut s = spawn("cat", Some(1000))?;
    /// s.send_line("hello, polly!")?;
    /// s.exp_any(vec![ReadUntil::String("hello".into()),
    ///                ReadUntil::EOF])?;
    ///         # Ok(())
    ///     # }().expect("test failed");
    /// # }
    /// ```
    pub async fn exp_any(&mut self, needles: Vec<ReadUntil>) -> Result<(String, String)> {
        self.exp(&ReadUntil::Any(needles)).await
    }
}

/// Spawn a REPL from a stream
pub async fn spawn_async_stream<
    R: tokio_stream::Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> + Unpin,
    W: AsyncWrite + Unpin,
>(
    reader: R,
    writer: W,
    timeout: Option<u64>,
) -> StreamSession<R, W> {
    StreamSession::new(reader, writer, timeout).await
}
