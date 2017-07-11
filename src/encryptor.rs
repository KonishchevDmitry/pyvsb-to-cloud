use std::fs::File;
use std::io::{self, Read, BufReader, BufRead, Write, BufWriter};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio, Child, ChildStdin, ChildStdout};
use std::thread::{self, JoinHandle};
use std::time;

use futures::{Future, Sink};
use futures::sync::mpsc;
use hyper;
use nix::{fcntl, unistd};

use core::{EmptyResult, GenericResult};
use provider::{ChunkReceiver, ChunkResult};
use util;

pub struct Encryptor {
    pid: i32,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout_reader: Option<JoinHandle<EmptyResult>>,
    encrypted_chunks_tx: Option<mpsc::Sender<ChunkResult>>,
    result: Option<EmptyResult>,
}

impl Encryptor {
    pub fn new(encryption_passphrase: &str) -> GenericResult<(Encryptor, ChunkReceiver)> {
        debug!("Spawning a gpg process to handle data encryption...");

        // Buffer is for the following reasons:
        // 1. Parallelization on successful result.
        // 2. To not block in drop() if we get some error during dropping the object that hasn't
        //    been used yet (hasn't been written to):
        //    * One buffer slot for gpg overhead around an empty payload.
        //    * One buffer slot for our error message.
        let (tx, rx) = mpsc::channel(2);

        let (passphrase_read_fd, passphrase_write_fd) = unistd::pipe2(fcntl::O_CLOEXEC)
            .map_err(|e| format!("Unable to create a pipe: {}", e))?;

        let (passphrase_read_fd, mut passphrase_write_fd) = unsafe {
            (File::from_raw_fd(passphrase_read_fd), File::from_raw_fd(passphrase_write_fd))
        };

        fcntl::fcntl(passphrase_read_fd.as_raw_fd(),
                     fcntl::FcntlArg::F_SETFD(fcntl::FdFlag::empty()))?;

        let mut gpg = Command::new("gpg")
            .arg("--batch").arg("--symmetric")
            .arg("--passphrase-fd").arg(passphrase_read_fd.as_raw_fd().to_string())
            .arg("--compress-algo").arg("none")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e| format!("Unable to spawn a gpg process: {}", e))?;

        let pid = gpg.id() as i32;
        let stdin = BufWriter::new(gpg.stdin.take().unwrap());
        let encrypted_chunks_tx = tx.clone();

        let stdout_reader = thread::Builder::new().name("gpg stdout reader".into()).spawn(move || {
            stdout_reader(gpg, tx)
        }).map_err(|e| {
            terminate_gpg(pid);
            format!("Unable to spawn a thread: {}", e)
        })?;

        let encryptor = Encryptor {
            pid: pid,
            stdin: Some(stdin),
            stdout_reader: Some(stdout_reader),
            encrypted_chunks_tx: Some(encrypted_chunks_tx),
            result: None,
        };

        if let Err(err) = passphrase_write_fd.write_all(encryption_passphrase.as_bytes())
            .and_then(|_| passphrase_write_fd.flush()) {
            encryptor.finish()?;
            return Err!("Failed to pass encryption passphrase to gpg: {}", err);
        }

        Ok((encryptor, rx))
    }

    pub fn finish(mut self) -> EmptyResult {
        self.close(Ok(()))
    }

    fn close(&mut self, mut result: EmptyResult) -> EmptyResult {
        if let None = self.result {
            debug!("Closing encryptor with {:?}...", result);

            if let Some(mut stdin) = self.stdin.take() {
                if let Err(err) = stdin.flush() {
                    result = Err(err.into());
                }

                // Here stdin will be dropped and thus closed, so the gpg process will be expected to
                // read the remaining data and finish its work as well as our stdout reading thread.
            }

            if let Some(stdout_reader) = self.stdout_reader.take() {
                if let Err(err) = util::join_thread(stdout_reader) {
                    result = Err(err.into());
                    terminate_gpg(self.pid);
                }
            }

            if let Some(encrypted_chunks_tx) = self.encrypted_chunks_tx.take() {
                if let Err(ref err) = result {
                    let _ = encrypted_chunks_tx.send(Err(
                        io_error_from_string(err.to_string()).into()));
                }
            }

            debug!("Encryptor has closed with {:?}.", result);
            self.result = Some(result);
        }

        match *self.result.as_ref().unwrap() {
            Ok(()) => Ok(()),
            Err(ref err) => Err(err.to_string().into()),
        }
    }
}

impl Drop for Encryptor {
    fn drop(&mut self) {
        let _ = self.close(Ok(()));
    }
}

impl io::Write for Encryptor {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(ref result) = self.result {
            return Err(io_error_from_string(result.as_ref().unwrap_err().to_string()));
        }

        self.stdin.as_mut().unwrap().write(buf).map_err(|e| {
            io_error_from_string(self.close(Err(e.into())).unwrap_err().to_string())
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(ref result) = self.result {
            return Err(io_error_from_string(result.as_ref().unwrap_err().to_string()));
        }

        self.stdin.as_mut().unwrap().flush().map_err(|e| {
            io_error_from_string(self.close(Err(e.into())).unwrap_err().to_string())
        })
    }
}

fn stdout_reader(mut gpg: Child, tx: mpsc::Sender<ChunkResult>) -> EmptyResult {
    let mut stderr = gpg.stderr.take().unwrap();
    let mut stderr_reader = Some(thread::Builder::new().name("gpg stderr reader".into()).spawn(move || -> EmptyResult {
        let mut error = String::new();

        match stderr.read_to_string(&mut error) {
            Ok(size) => {
                if size == 0 {
                    Ok(())
                } else {
                    Err!("gpg error: {}", error.trim_right())
                }
            },
            Err(err) => Err!("gpg stderr reading error: {}", err),
        }
    }).map_err(|err| format!("Unable to spawn a thread: {}", err))?);

    let stdout = BufReader::new(gpg.stdout.take().unwrap());

    read_data(stdout, tx).map_err(|err| {
        terminate_gpg(gpg.id() as i32);
        util::join_thread_ignoring_result(stderr_reader.take().unwrap());
        err
    })?;

    util::join_thread(stderr_reader.take().unwrap())?;

    let status = gpg.wait().map_err(|e| format!("Failed to wait() a child gpg process: {}", e))?;
    if status.success() {
        debug!("gpg process has terminated with successful exit code.")
    } else {
        return Err!("gpg process has terminated with an error exit code")
    }

    Ok(())
}

fn read_data(mut stdout: BufReader<ChildStdout>, mut tx: mpsc::Sender<ChunkResult>) -> EmptyResult {
    // FIXME
    let mut out = File::create("backup-mock.tar.gpg").unwrap();

    loop {
        let size = {
            let data = stdout.fill_buf().map_err(|e| format!("gpg stdout reading error: {}", e))?;
            if data.len() == 0 {
                break;
            }

            out.write_all(data)?;

            // FIXME
            tx = tx.send(Ok(data.to_vec().into())).wait().map_err(|e| {
                error!("Receiver has closed connection");
                format!("Connection is closed: {}", e)
            })?;

            data.len()
        };
        stdout.consume(size);
    }

    Ok(())
}

fn terminate_gpg(pid: i32) {
    let termination_timeout = time::Duration::from_secs(3);
    if let Err(err) = util::terminate_process("a child gpg process", pid, termination_timeout) {
        error!("{}.", err)
    }
}

fn io_error_from_string(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error)
}