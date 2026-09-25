use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::protocol::{Request, Response, ServerMessage};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(
        "loom core is not running ({}); start it with `loom` inside a Zellij session",
        .0.display()
    )]
    CoreNotRunning(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed by server")]
    ConnectionClosed,
    #[error("server error: {0}")]
    Server(String),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(socket_path: &Path) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(socket_path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::CoreNotRunning(socket_path.to_path_buf())
            }
            _ => ClientError::Io(e),
        })?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn write_request(&mut self, request: &Request) -> Result<(), ClientError> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn read_response(&mut self) -> Result<Response, ClientError> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Err(ClientError::ConnectionClosed);
        }
        Ok(serde_json::from_str(line.trim_end())?)
    }

    pub fn read_message(&mut self) -> Result<ServerMessage, ClientError> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Err(ClientError::ConnectionClosed);
        }
        Ok(serde_json::from_str(line.trim_end())?)
    }

    pub fn call(&mut self, request: &Request) -> Result<Response, ClientError> {
        self.write_request(request)?;
        self.read_response()
    }

    pub fn set_read_timeout(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> Result<(), ClientError> {
        self.reader.get_ref().set_read_timeout(timeout)?;
        Ok(())
    }
}

pub fn call(socket_path: &Path, request: &Request) -> Result<serde_json::Value, ClientError> {
    let mut client = Client::connect(socket_path)?;
    let response = client.call(request)?;
    response
        .into_result()
        .map(|data| data.unwrap_or(serde_json::Value::Null))
        .map_err(ClientError::Server)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_message(socket_path: &Path) -> String {
        format!(
            "loom core is not running ({}); start it with `loom` inside a Zellij session",
            socket_path.display()
        )
    }

    #[test]
    fn missing_socket_reports_core_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("missing.sock");
        let err = call(&socket_path, &Request::List).unwrap_err();
        assert!(matches!(err, ClientError::CoreNotRunning(_)));
        assert_eq!(err.to_string(), expected_message(&socket_path));
    }

    #[test]
    fn refused_socket_reports_core_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("refused.sock");
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = socket_path.as_os_str().as_encoded_bytes();
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        let bound = unsafe {
            libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        assert_eq!(bound, 0);
        let err = call(&socket_path, &Request::List).unwrap_err();
        unsafe { libc::close(fd) };
        assert_eq!(err.to_string(), expected_message(&socket_path));
    }
}
