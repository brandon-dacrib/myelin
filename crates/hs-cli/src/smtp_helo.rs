//! A last-resort plain-SMTP sender for a relay that does not speak ESMTP.
//!
//! The SMTP client email pushers and validation emails use (`lettre`, under
//! `hs_push::email::smtp`) opens with `EHLO` and gives up when the server answers it with
//! `500`/`502`. RFC 5321 section 4.1.4 says a client SHOULD then fall back to `HELO`, which is
//! all an old relay or a minimal test mail catcher understands: Sytest's mail server is one
//! (`lib/SyTest/MailServer/Protocol.pm` answers `HELO`, `MAIL`, `RCPT`, `DATA` and nothing else),
//! and every validation email to it failed "500 Syntax error: unrecognized command". This module
//! is that fallback, for connections without TLS only (`email.smtp.security: none`): `HELO`,
//! `MAIL FROM`, `RCPT TO`, `DATA` with dot-stuffing, `QUIT`. No authentication, no extensions.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long the whole exchange may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The bare address of a mailbox written `Name <address>` or `address`.
#[must_use]
pub fn bare_address(mailbox: &str) -> &str {
    match (mailbox.rfind('<'), mailbox.rfind('>')) {
        (Some(open), Some(close)) if open < close => &mailbox[open + 1..close],
        _ => mailbox.trim(),
    }
}

/// `message` (the whole RFC 5322 text, CRLF line endings) with every line starting with `.`
/// doubled, as `DATA` requires.
#[must_use]
pub fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 16);
    let mut at_line_start = true;
    for &byte in message {
        if at_line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        at_line_start = byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Sends `message` from `from` to `to` through `host:port` with plain `HELO`.
///
/// # Errors
/// Why the server could not be reached or refused a step.
pub async fn send(
    host: &str,
    port: u16,
    from: &str,
    to: &str,
    message: &[u8],
) -> Result<(), String> {
    tokio::time::timeout(TIMEOUT, exchange(host, port, from, to, message))
        .await
        .map_err(|_| format!("the SMTP exchange with {host}:{port} timed out"))?
}

async fn exchange(
    host: &str,
    port: u16,
    from: &str,
    to: &str,
    message: &[u8],
) -> Result<(), String> {
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("could not connect to {host}:{port}: {e}"))?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    expect(&mut read, &[220]).await?;
    let steps: [(String, &[u16]); 3] = [
        ("HELO localhost\r\n".to_owned(), &[250]),
        (format!("MAIL FROM:<{}>\r\n", bare_address(from)), &[250]),
        (format!("RCPT TO:<{}>\r\n", bare_address(to)), &[250, 251]),
    ];
    for (command, ok) in steps {
        write
            .write_all(command.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        expect(&mut read, ok).await?;
    }
    write
        .write_all(b"DATA\r\n")
        .await
        .map_err(|e| e.to_string())?;
    expect(&mut read, &[354]).await?;
    write
        .write_all(&dot_stuffed(message))
        .await
        .map_err(|e| e.to_string())?;
    write.write_all(b".\r\n").await.map_err(|e| e.to_string())?;
    expect(&mut read, &[250]).await?;
    let _ = write.write_all(b"QUIT\r\n").await;
    Ok(())
}

/// Reads one (possibly multi-line) reply and checks its code.
async fn expect<R: tokio::io::AsyncBufRead + Unpin>(
    read: &mut R,
    ok: &[u16],
) -> Result<(), String> {
    loop {
        let mut line = String::new();
        let n = read.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the SMTP server closed the connection".to_owned());
        }
        let code: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| format!("not an SMTP reply: {}", line.trim_end()))?;
        // `250-...` continues a multi-line reply; `250 ...` ends it.
        if line.as_bytes().get(3) == Some(&b'-') {
            continue;
        }
        return if ok.contains(&code) {
            Ok(())
        } else {
            Err(format!("the SMTP server answered {}", line.trim_end()))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_dots() {
        assert_eq!(
            bare_address("Matrix <noreply@example.org>"),
            "noreply@example.org"
        );
        assert_eq!(bare_address(" bob@example.com "), "bob@example.com");
        assert_eq!(dot_stuffed(b"a\r\n.b\r\n"), b"a\r\n..b\r\n".to_vec());
        assert_eq!(dot_stuffed(b".x"), b"..x\r\n".to_vec());
    }

    /// A server that knows only `HELO`, like Sytest's.
    #[tokio::test]
    async fn sends_to_a_server_that_only_knows_helo() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            write.write_all(b"220 hi\r\n").await.unwrap();
            let mut data = String::new();
            let mut in_data = false;
            loop {
                let mut line = String::new();
                if read.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                if in_data {
                    if line == ".\r\n" {
                        in_data = false;
                        write.write_all(b"250 ok\r\n").await.unwrap();
                    } else {
                        data.push_str(&line);
                    }
                    continue;
                }
                let reply: &[u8] = match line.split_whitespace().next().unwrap_or("") {
                    "HELO" | "MAIL" | "RCPT" => b"250 ok\r\n",
                    "DATA" => {
                        in_data = true;
                        b"354 go\r\n"
                    }
                    "QUIT" => break,
                    _ => b"500 Syntax error: unrecognized command\r\n",
                };
                write.write_all(reply).await.unwrap();
            }
            data
        });
        send(
            "127.0.0.1",
            port,
            "Matrix <noreply@example.org>",
            "bob@example.com",
            b"Subject: s\r\n\r\n.dot\r\nbody\r\n",
        )
        .await
        .unwrap();
        let data = server.await.unwrap();
        assert!(data.contains("..dot\r\n"), "{data}");
        assert!(data.contains("body\r\n"), "{data}");
    }
}
