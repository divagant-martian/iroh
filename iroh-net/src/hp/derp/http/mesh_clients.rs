use reqwest::Url;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::hp::{
    derp::{http::ClientBuilder, DerpMap, MeshKey, PacketForwarderHandler},
    key::node::SecretKey,
};

use super::Client;

/// Spawns, connects, and manages special `derp::http::Clients`.
///
/// These clients handled incoming network update notifications from remote
/// `derp::http::Server`s. These servers are used as `PacketForwarder`s for
/// peers to which we are not directly connected.
/// A `mesh_key` is used to ensure the remote server belongs to the same mesh network.
#[derive(Debug)]
pub(crate) struct MeshClients {
    tasks: JoinSet<()>,
    mesh_key: MeshKey,
    server_key: SecretKey,
    mesh_addrs: MeshAddrs,
    packet_fwd: PacketForwarderHandler<Client>,
    cancel: CancellationToken,
}

impl MeshClients {
    pub(crate) fn new(
        mesh_key: MeshKey,
        server_key: SecretKey,
        mesh_addrs: MeshAddrs,
        packet_fwd: PacketForwarderHandler<Client>,
    ) -> Self {
        Self {
            tasks: JoinSet::new(),
            cancel: CancellationToken::new(),
            mesh_key,
            server_key,
            mesh_addrs,
            packet_fwd,
        }
    }

    pub(crate) async fn mesh(&mut self) {
        let addrs = match &self.mesh_addrs {
            MeshAddrs::Addrs(urls) => urls.to_owned(),
            MeshAddrs::DerpMap(derp_map) => {
                let mut urls = Vec::new();
                for (_, region) in derp_map.regions.iter() {
                    for node in region.nodes.iter() {
                        // note: `node.host_name` is expected to include the scheme
                        let mut url = node.url.clone();
                        url.set_path("/derp");
                        urls.push(url);
                    }
                }
                urls
            }
        };
        for addr in addrs {
            let client = ClientBuilder::new()
                .mesh_key(Some(self.mesh_key))
                .server_url(addr)
                .build(self.server_key.clone())
                .expect("will only fail if no `server_url` is present");

            let packet_forwarder_handler = self.packet_fwd.clone();
            self.tasks.spawn(async move {
                if let Err(e) = client.run_mesh_client(packet_forwarder_handler).await {
                    tracing::warn!("{e:?}");
                }
            });
        }
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancel.cancel();
        self.tasks.shutdown().await
    }
}

#[derive(Debug, Clone)]
/// The different ways to express the mesh network you want to join.
pub enum MeshAddrs {
    /// Supply a `DerpMap` of all the derp servers you want to mesh with.
    DerpMap(DerpMap),
    /// Supply a list of `Url`s of all the derp server you want to mesh with.
    Addrs(Vec<Url>),
}

#[cfg(test)]
mod tests {
    use crate::hp::derp::{http::ServerBuilder, ReceivedMessage};
    use anyhow::{bail, Result};
    use tracing_subscriber::{prelude::*, EnvFilter};

    use super::*;

    #[tokio::test]
    async fn test_mesh_network() -> Result<()> {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(EnvFilter::from_default_env())
            .try_init()
            .ok();

        let mesh_key: MeshKey = [1; 32];
        let a_key = SecretKey::generate();
        tracing::info!("derp server a: {:?}", a_key.public_key());
        let mut derp_server_a = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
            .secret_key(Some(a_key))
            .mesh_key(Some(mesh_key))
            .spawn()
            .await?;

        let b_key = SecretKey::generate();
        tracing::info!("derp server b: {:?}", b_key.public_key());
        let mut derp_server_b = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
            .secret_key(Some(b_key))
            .mesh_key(Some(mesh_key))
            .spawn()
            .await?;

        let a_url: Url = format!("http://{}/derp", derp_server_a.addr())
            .parse()
            .unwrap();
        let b_url: Url = format!("http://{}/derp", derp_server_b.addr())
            .parse()
            .unwrap();

        derp_server_a
            .re_mesh(MeshAddrs::Addrs(vec![b_url.clone()]))
            .await?;
        derp_server_b
            .re_mesh(MeshAddrs::Addrs(vec![a_url.clone()]))
            .await?;

        let alice_key = SecretKey::generate();
        tracing::info!("client alice: {:?}", alice_key.public_key());
        let alice = ClientBuilder::new()
            .server_url(a_url)
            .build(alice_key.clone())?;
        let _ = alice.connect().await?;

        let bob_key = SecretKey::generate();
        tracing::info!("client bob: {:?}", bob_key.public_key());
        let bob = ClientBuilder::new()
            .server_url(b_url)
            .build(bob_key.clone())?;
        let _ = bob.connect().await?;

        // needs time for the mesh network to fully mesh
        // packets may get dropped the first go-around
        // this will loop until we get the first keepalive, message, which means
        // there is really something wrong if we can't gt
        // send bob a message from alice
        let msg = "howdy, bob!";
        tracing::info!("send message from alice to bob");
        alice.send(bob_key.public_key(), msg.into()).await?;

        loop {
            tokio::select! {
                recv = bob.recv_detail() => {
                    let (recv, _) = recv?;
                    if let ReceivedMessage::ReceivedPacket { source, data } = recv {
                        assert_eq!(alice_key.public_key(), source);
                        assert_eq!(msg, data);
                        break;
                    } else {
                        bail!("unexpected ReceivedMessage {recv:?}");
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    tracing::info!("attempting to send another message from alice to bob");
                    alice.send(bob_key.public_key(), msg.into()).await?;
                }
            }
        }

        // send alice a message from bob
        let msg = "why hello, alice!";
        tracing::info!("send message from bob to alice");
        bob.send(alice_key.public_key(), msg.into()).await?;

        let (recv, _) = alice.recv_detail().await?;
        if let ReceivedMessage::ReceivedPacket { source, data } = recv {
            assert_eq!(bob_key.public_key(), source);
            assert_eq!(msg, data);
        } else {
            bail!("unexpected ReceivedMessage {recv:?}");
        }

        // shutdown the servers
        derp_server_a.shutdown().await;
        derp_server_b.shutdown().await;
        Ok(())
    }
}