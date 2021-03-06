//! Local server that establish a TCP tunnel with server

use std::{
    io,
    io::ErrorKind,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures::future::{self, Either};
use log::{debug, error, info, trace};
use tokio::net::{TcpListener, TcpStream};

use crate::{
    config::ServerConfig,
    context::{Context, SharedContext},
    relay::{
        loadbalancing::server::{LoadBalancer, PingBalancer, PingServer, PingServerType},
        socks5::Address,
    },
};

/// Established Client Tunnel
///
/// This method must be called after handshaking with client (for example, socks5 handshaking)
async fn establish_client_tcp_tunnel<'a>(
    context: &Context,
    mut s: TcpStream,
    client_addr: SocketAddr,
    addr: &Address,
    svr_cfg: &ServerConfig,
) -> io::Result<()> {
    let svr_s = match super::connect_proxy_server(context, &*svr_cfg).await {
        Ok(svr_s) => {
            trace!("Proxy server connected, {:?}", svr_cfg);
            svr_s
        }
        Err(err) => {
            // Just close the connection.
            error!("Failed to connect remote server {}, err: {}", svr_cfg.addr(), err);
            return Err(err);
        }
    };

    let mut svr_s = super::proxy_server_handshake(context, svr_s, svr_cfg, addr).await?;
    let (mut svr_r, mut svr_w) = svr_s.split();

    let (mut r, mut w) = s.split();

    use tokio::io::copy;

    let rhalf = copy(&mut r, &mut svr_w);
    let whalf = copy(&mut svr_r, &mut w);

    debug!(
        "TUNNEL relay established {} <-> {} ({})",
        client_addr,
        svr_cfg.addr(),
        addr
    );

    match future::select(rhalf, whalf).await {
        Either::Left((Ok(..), _)) => trace!("TUNNEL relay {} -> {} ({}) closed", client_addr, svr_cfg.addr(), addr),
        Either::Left((Err(err), _)) => {
            if let ErrorKind::TimedOut = err.kind() {
                trace!(
                    "TUNNEL relay {} -> {} ({}) closed with error {}",
                    client_addr,
                    svr_cfg.addr(),
                    addr,
                    err,
                );
            } else {
                error!(
                    "TUNNEL relay {} -> {} ({}) closed with error {}",
                    client_addr,
                    svr_cfg.addr(),
                    addr,
                    err,
                );
            }
        }
        Either::Right((Ok(..), _)) => trace!("TUNNEL relay {} <- {} ({}) closed", client_addr, svr_cfg.addr(), addr),
        Either::Right((Err(err), _)) => {
            if let ErrorKind::TimedOut = err.kind() {
                trace!(
                    "TUNNEL relay {} <- {} ({}) closed with error {}",
                    client_addr,
                    svr_cfg.addr(),
                    addr,
                    err,
                );
            } else {
                error!(
                    "TUNNEL relay {} <- {} ({}) closed with error {}",
                    client_addr,
                    svr_cfg.addr(),
                    addr,
                    err,
                );
            }
        }
    }

    debug!("TUNNEL relay {} <-> {} ({}) closed", client_addr, svr_cfg.addr(), addr);

    Ok(())
}

async fn handle_tunnel_client(context: &Context, s: TcpStream, server_score: Arc<ServerScore>) -> io::Result<()> {
    let conf = server_score.server_config();

    if let Err(err) = s.set_keepalive(conf.timeout()) {
        error!("Failed to set keep alive: {:?}", err);
    }

    if context.config().no_delay {
        if let Err(err) = s.set_nodelay(true) {
            error!("Failed to set no delay: {:?}", err);
        }
    }

    let client_addr = s.peer_addr()?;

    // forward must not be None, it is already checked in local.rs
    let target_addr = context.config().forward.as_ref().unwrap();

    establish_client_tcp_tunnel(context, s, client_addr, target_addr, conf).await
}

struct ServerScore {
    svr_cfg: ServerConfig,
    score: AtomicU64,
}

impl ServerScore {
    fn new(config: &ServerConfig) -> Arc<ServerScore> {
        let s = ServerScore {
            svr_cfg: config.clone(),
            score: AtomicU64::new(0),
        };
        Arc::new(s)
    }
}

impl PingServer for ServerScore {
    fn server_config(&self) -> &ServerConfig {
        &self.svr_cfg
    }

    fn score(&self) -> u64 {
        self.score.load(Ordering::Acquire)
    }

    fn set_score(&self, score: u64) {
        self.score.store(score, Ordering::Release);
    }
}

pub async fn run(context: SharedContext) -> io::Result<()> {
    assert!(
        context.config().mode.enable_tcp(),
        "You must enable TCP relay for tunneling"
    );

    let local_addr = context.config().local.as_ref().expect("Missing local config");
    let bind_addr = local_addr.bind_addr(&*context).await?;

    let mut listener = TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|err| panic!("Failed to listen on {}, {}", local_addr, err));

    let actual_local_addr = listener.local_addr().expect("Could not determine port bound to");

    let servers = context.config().server.iter().map(ServerScore::new).collect();
    let mut servers = PingBalancer::new(context.clone(), servers, PingServerType::Tcp).await;
    info!(
        "ShadowSocks TCP Tunnel Listening on {}, forward to {}",
        actual_local_addr,
        context.config().forward.as_ref().unwrap()
    );

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let server_cfg = servers.pick_server();

        trace!("Got connection, addr: {}", peer_addr);
        trace!("Picked proxy server: {:?}", server_cfg.server_config());

        let context = context.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_tunnel_client(&*context, socket, server_cfg).await {
                error!("TCP Tunnel client {}", err);
            }
        });
    }
}
