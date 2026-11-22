//! A switch between a client and a fake: a loopback TCP forwarder that can
//! be taken down (the port refuses connections, every open connection is
//! dropped) and brought back on the same port.
//!
//! That is what "the network went away" looks like to an adapter: a connect
//! error, not a scripted status. Loopback only.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinHandle};

#[derive(Default)]
struct Inner {
    listener: Option<JoinHandle<()>>,
    connections: Vec<AbortHandle>,
}

/// Forwards `127.0.0.1:<port>` to a fake. See the module doc.
pub struct LinkGate {
    addr: SocketAddr,
    target: SocketAddr,
    inner: Arc<Mutex<Inner>>,
}

impl LinkGate {
    /// Listens on a loopback port the system picks and forwards to
    /// `target`.
    pub async fn start(target: SocketAddr) -> LinkGate {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local address");
        let gate = LinkGate {
            addr,
            target,
            inner: Arc::new(Mutex::new(Inner::default())),
        };
        gate.serve(listener);
        gate
    }

    /// Like [`LinkGate::start`], taking the fake's base URL
    /// (`http://127.0.0.1:<port>`).
    pub async fn in_front_of(base_url: &str) -> LinkGate {
        let target = base_url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .parse()
            .expect("a fake's base URL is http://127.0.0.1:<port>");
        Self::start(target).await
    }

    /// What to give the adapter instead of the fake's own URL.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The network goes away: the port refuses connections and every open
    /// connection is cut.
    pub fn down(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(listener) = inner.listener.take() {
            listener.abort();
        }
        for connection in inner.connections.drain(..) {
            connection.abort();
        }
    }

    /// The network is back, on the same port. Panics if the port was taken
    /// meanwhile.
    pub async fn up(&self) {
        if self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listener
            .is_some()
        {
            return;
        }
        // The aborted listener task drops its socket when it is next
        // polled: give it a moment before binding the port again.
        let mut last = None;
        for _ in 0..50 {
            match TcpListener::bind(self.addr).await {
                Ok(listener) => {
                    self.serve(listener);
                    return;
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }
        panic!(
            "the gate's port {} could not be bound again: {last:?}",
            self.addr
        );
    }

    fn serve(&self, listener: TcpListener) {
        let target = self.target;
        let inner = self.inner.clone();
        let accepting = self.inner.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let connection = tokio::spawn(async move {
                    if let Ok(mut upstream) = TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                });
                let mut inner = accepting.lock().unwrap_or_else(|e| e.into_inner());
                inner.connections.retain(|c| !c.is_finished());
                inner.connections.push(connection.abort_handle());
            }
        });
        inner.lock().unwrap_or_else(|e| e.into_inner()).listener = Some(task);
    }
}

impl Drop for LinkGate {
    fn drop(&mut self) {
        self.down();
    }
}
