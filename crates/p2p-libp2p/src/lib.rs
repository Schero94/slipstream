//! rust-libp2p transport for the versioned Slipstream inference protocol.

#![forbid(unsafe_code)]

use std::convert::Infallible;
use std::io;
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::NetworkBehaviour;
use libp2p::{StreamProtocol, Swarm, SwarmBuilder};

pub const INFERENCE_PROTOCOL_V1: &str = "/slipstream/inference/1";
pub const MAX_MESH_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct BoundedBytesCodec;

#[async_trait]
impl request_response::Codec for BoundedBytesCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_bounded(io).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_bounded(io).await
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_bounded(io, request).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_bounded(io, response).await
    }
}

async fn read_bounded<T>(io: &mut T) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut prefix = [0u8; 4];
    io.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_MESH_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mesh message length outside allowed range",
        ));
    }
    let mut message = vec![0u8; length];
    io.read_exact(&mut message).await?;
    Ok(message)
}

async fn write_bounded<T>(io: &mut T, message: Vec<u8>) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    if message.is_empty() || message.len() > MAX_MESH_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mesh message length outside allowed range",
        ));
    }
    let length = u32::try_from(message.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mesh message too large"))?;
    io.write_all(&length.to_be_bytes()).await?;
    io.write_all(&message).await?;
    io.close().await?;
    Ok(())
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "MeshEvent")]
pub struct MeshBehaviour {
    pub inference: request_response::Behaviour<BoundedBytesCodec>,
}

impl MeshBehaviour {
    pub fn new() -> Self {
        let protocols = [(
            StreamProtocol::new(INFERENCE_PROTOCOL_V1),
            ProtocolSupport::Full,
        )];
        Self {
            inference: request_response::Behaviour::with_codec(
                BoundedBytesCodec,
                protocols,
                request_response::Config::default()
                    .with_request_timeout(Duration::from_secs(60 * 60)),
            ),
        }
    }

    pub fn send_request(
        &mut self,
        peer: &libp2p::PeerId,
        request: Vec<u8>,
    ) -> request_response::OutboundRequestId {
        self.inference.send_request(peer, request)
    }

    pub fn send_response(
        &mut self,
        channel: request_response::ResponseChannel<Vec<u8>>,
        response: Vec<u8>,
    ) -> Result<(), Vec<u8>> {
        self.inference.send_response(channel, response)
    }
}

impl Default for MeshBehaviour {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum MeshEvent {
    Inference(request_response::Event<Vec<u8>, Vec<u8>>),
}

impl From<request_response::Event<Vec<u8>, Vec<u8>>> for MeshEvent {
    fn from(event: request_response::Event<Vec<u8>, Vec<u8>>) -> Self {
        Self::Inference(event)
    }
}

/// Construct a QUIC swarm with an authenticated ephemeral libp2p identity.
/// Persistent identity injection is added by the node integration layer.
pub fn build_quic_swarm() -> Result<Swarm<MeshBehaviour>, Infallible> {
    build_quic_swarm_with_identity(libp2p::identity::Keypair::generate_ed25519())
}

/// Construct a QUIC swarm with a caller-provided persistent identity.
pub fn build_quic_swarm_with_identity(
    identity: libp2p::identity::Keypair,
) -> Result<Swarm<MeshBehaviour>, Infallible> {
    let swarm = SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_quic()
        .with_behaviour(|_| MeshBehaviour::new())
        .expect("infallible mesh behaviour")
        .build();
    Ok(swarm)
}
