use anyhow::{Context, Result, anyhow};
use std::net::{IpAddr, SocketAddr};
use tokio::net::TcpListener;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Bind a TCP listener for the API path with IPv6-first behavior.
///
/// If `listen` is `None`, this tries a dual-stack IPv6 socket on `[::]:port`
/// and falls back to `0.0.0.0:port` when IPv6 is unavailable.
///
/// # Errors
///
/// Returns an error if the provided IP is invalid or binding fails.
pub async fn bind_tcp_listener(port: u16, listen: Option<String>) -> Result<(TcpListener, String)> {
    if let Some(addr) = listen {
        let ip = addr.parse::<IpAddr>().map_err(|_| {
            anyhow!(
                "Invalid IP address: '{addr}'. Expected IPv4 (e.g. 0.0.0.0, 127.0.0.1) or IPv6 (e.g. ::, ::1)"
            )
        })?;

        return bind_explicit_listener(ip, port)
            .with_context(|| format!("failed to bind explicit API listener to {ip}:{port}"));
    }

    if let Ok(listener) = bind_dual_stack_listener(port) {
        let display = format_socket_addr(listener.local_addr()?);
        return Ok((listener, display));
    }

    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
        .await
        .with_context(|| format!("failed to bind IPv4 API listener to 0.0.0.0:{port}"))?;
    let display = format_socket_addr(listener.local_addr()?);

    Ok((listener, display))
}

fn bind_explicit_listener(ip: IpAddr, port: u16) -> Result<(TcpListener, String)> {
    let socket_addr = SocketAddr::new(ip, port);
    let listener = match ip {
        IpAddr::V4(_) => bind_socket_listener(Domain::IPV4, socket_addr, None)?,
        IpAddr::V6(_) => bind_socket_listener(Domain::IPV6, socket_addr, Some(true))?,
    };

    let display = format_socket_addr(listener.local_addr()?);
    Ok((listener, display))
}

fn bind_dual_stack_listener(port: u16) -> Result<TcpListener> {
    bind_socket_listener(
        Domain::IPV6,
        SocketAddr::from(([0_u16; 8], port)),
        Some(false),
    )
}

fn bind_socket_listener(
    domain: Domain,
    socket_addr: SocketAddr,
    only_v6: Option<bool>,
) -> Result<TcpListener> {
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;

    if let Some(only_v6) = only_v6 {
        socket.set_only_v6(only_v6)?;
    }

    socket.bind(&SockAddr::from(socket_addr))?;
    socket.listen(1_024)?;
    socket.set_nonblocking(true)?;

    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener).map_err(Into::into)
}

fn format_socket_addr(addr: SocketAddr) -> String {
    let ip = addr.ip();
    let port = addr.port();

    if ip.is_ipv6() {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::bind_tcp_listener;
    use anyhow::Result;

    #[tokio::test]
    async fn binds_explicit_ipv4_listener() -> Result<()> {
        let (listener, display) = bind_tcp_listener(0, Some("127.0.0.1".to_owned())).await?;
        let local_addr = listener.local_addr()?;

        assert!(local_addr.is_ipv4());
        assert!(display.starts_with("127.0.0.1:"));

        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_listen_ip() {
        let result = bind_tcp_listener(0, Some("invalid-ip".to_owned())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn auto_bind_prefers_reachable_listener() -> Result<()> {
        let (listener, display) = bind_tcp_listener(0, None).await?;
        let local_addr = listener.local_addr()?;

        assert!(local_addr.ip().is_ipv4() || local_addr.ip().is_ipv6());
        assert!(display.contains(':'));

        Ok(())
    }
}
