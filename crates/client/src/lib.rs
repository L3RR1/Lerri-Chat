//! Orquestra rede, mídia e criptografia para a UI (SPEC-01 §3). Na Fase 0:
//! conectar a um relay e trocar um `Echo`.

use std::net::SocketAddr;

use anyhow::{Context, bail};
use lerri_core::protocol::{Frame, RELAY_SERVER_NAME};
use lerri_net::Fingerprint;
use lerri_net::framing::{read_frame, write_frame};
use lerri_net::quinn::{Connection, Endpoint};

pub struct RelayConnection {
    endpoint: Endpoint,
    conn: Connection,
}

impl RelayConnection {
    /// Conecta ao relay em `addr`, aceitando só o certificado `fingerprint`.
    pub async fn connect(addr: SocketAddr, fingerprint: Fingerprint) -> anyhow::Result<Self> {
        let endpoint = lerri_net::client_endpoint(fingerprint)?;
        let conn = endpoint
            .connect(addr, RELAY_SERVER_NAME)?
            .await
            .with_context(|| format!("falha ao conectar em {addr}"))?;
        Ok(Self { endpoint, conn })
    }

    pub fn rtt(&self) -> std::time::Duration {
        self.conn.rtt()
    }

    /// Envia `payload` e espera o relay devolver o mesmo conteúdo.
    pub async fn echo(&self, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        write_frame(&mut send, &Frame::Echo { payload: payload.to_vec() }).await?;
        send.finish()?;
        match read_frame(&mut recv).await? {
            Some(Frame::Echo { payload }) => Ok(payload),
            None => bail!("relay fechou o stream sem responder"),
        }
    }

    pub async fn close(self) {
        self.conn.close(0u32.into(), b"tchau");
        self.endpoint.wait_idle().await;
    }
}
