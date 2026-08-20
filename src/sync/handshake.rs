//! Hello handshake: build-fingerprint check over the single transfer stream.
//! Runs first, before any data flows. No feature negotiation — both peers
//! must run the same build (no released v1 locks the wire format; the
//! fingerprint of the source tree is the version).

use crate::protocol::{BUILD_FINGERPRINT, Frame, stream};
use crate::{Error, Result};
use tokio::io::{AsyncRead, AsyncWrite};

/// Client side: announce our build fingerprint, await the server's reply.
pub(crate) async fn client<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    ctrl_send: &mut W,
    ctrl_recv: &mut R,
) -> Result<()> {
    stream::send_frame(
        ctrl_send,
        &Frame::Hello {
            fingerprint: BUILD_FINGERPRINT.to_string(),
        },
    )
    .await?;
    match stream::receive_frame(ctrl_recv).await? {
        Frame::HelloAck { accepted: true, .. } => Ok(()),
        Frame::HelloAck {
            accepted: false,
            fingerprint,
        } => Err(Error::HandshakeRejected {
            peer_build: fingerprint,
        }),
        Frame::Error { message } => {
            Err(Error::Other(format!("Peer rejected handshake: {message}")))
        }
        _ => Err(Error::Other("Expected HelloAck".to_string())),
    }
}

/// Server side: await the client's Hello, reply accept/reject.
pub(crate) async fn server<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    ctrl_send: &mut W,
    ctrl_recv: &mut R,
) -> Result<()> {
    match stream::receive_frame(ctrl_recv).await? {
        Frame::Hello { fingerprint } => {
            tracing::info!("peer hello: build={fingerprint}");
            let accepted = fingerprint == BUILD_FINGERPRINT;
            stream::send_frame(
                ctrl_send,
                &Frame::HelloAck {
                    fingerprint: BUILD_FINGERPRINT.to_string(),
                    accepted,
                },
            )
            .await?;
            if !accepted {
                return Err(Error::Other(format!(
                    "Peer build {fingerprint} incompatible with local build {BUILD_FINGERPRINT}"
                )));
            }
            Ok(())
        }
        Frame::Error { message } => Err(Error::Other(format!("Peer error: {message}"))),
        _ => Err(Error::Other("Expected Hello".to_string())),
    }
}
