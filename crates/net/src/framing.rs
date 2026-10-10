//! Leitura/escrita de [`Frame`]s em streams QUIC confiáveis.

use iroh::endpoint::{ReadExactError, RecvStream, SendStream};
use lerri_core::protocol::{Frame, MAX_FRAME_LEN};

pub async fn write_frame(send: &mut SendStream, frame: &Frame) -> anyhow::Result<()> {
    send.write_all(&frame.encode()?).await?;
    Ok(())
}

/// Lê o próximo frame. `Ok(None)` quando o peer fechou o stream de forma
/// limpa entre dois frames.
pub async fn read_frame(recv: &mut RecvStream) -> anyhow::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        anyhow::bail!("peer anunciou frame de {len} bytes (máximo {MAX_FRAME_LEN})");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(Some(Frame::decode_body(&body)?))
}
