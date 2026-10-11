//! Canal de voz de uma sala (Fase 3): liga o pipeline de [`lerri_media`] à
//! conexão com o host (datagramas QUIC). As chaves SFrame são da sala e
//! ficam com a sala (`room::run`), que as troca a cada epoch MLS.

use std::sync::Arc;

use lerri_media::Voice;
use lerri_media::screen::Keys;
use lerri_media::device::{DeviceChoice, Devices};
use lerri_net::iroh::endpoint::Connection;
use tokio::task::JoinHandle;

/// Voz ativa: o pipeline, os dispositivos (se houver) e a leitura de
/// datagramas. Para tudo ao ser derrubada.
pub struct VoiceSession {
    voice: Arc<Voice>,
    devices: Option<Devices>,
    reader: JoinHandle<()>,
    conn: Connection,
    /// `(datagramas UDP enviados, pacotes perdidos)` na última medição.
    last_stats: (u64, u64),
}

impl VoiceSession {
    /// `keys`: chaves SFrame da sala. `devices: false` não abre microfone nem
    /// saída: quem chama alimenta o [`Voice`] (testes).
    pub fn start(conn: Connection, keys: Keys, devices: bool, strong_ns: bool, choice: &DeviceChoice) -> anyhow::Result<Self> {
        let c = conn.clone();
        let voice = Voice::new(keys, move |d| {
            // Erro aqui é conexão caindo ou datagrama grande demais; a sessão
            // percebe o primeiro pelo stream de controle.
            let _ = c.send_datagram(d);
        });
        let devices = if devices { Some(Devices::start(voice.clone(), strong_ns, choice)?) } else { None };
        let (c, v) = (conn.clone(), voice.clone());
        let reader = tokio::spawn(async move {
            while let Ok(d) = c.read_datagram().await {
                v.receive(&d);
            }
        });
        let stats = conn.stats();
        Ok(Self { voice, devices, reader, conn, last_stats: (stats.udp_tx.datagrams, stats.lost_packets) })
    }

    pub fn voice(&self) -> &Arc<Voice> {
        &self.voice
    }

    pub fn devices(&self) -> Option<&Devices> {
        self.devices.as_ref()
    }

    /// Troca o microfone ou a saída sem sair da voz. Fecha os atuais antes
    /// (o WASAPI não gosta do mesmo dispositivo aberto duas vezes).
    pub fn restart_devices(&mut self, strong_ns: bool, choice: &DeviceChoice) -> anyhow::Result<()> {
        if self.devices.take().is_some() {
            self.devices = Some(Devices::start(self.voice.clone(), strong_ns, choice)?);
        }
        Ok(())
    }

    /// Perda de pacotes da conexão desde a última chamada, para o encoder se
    /// ajustar (SPEC-03 §6.1).
    pub fn measure_loss(&mut self) -> f32 {
        let s = self.conn.stats();
        let (sent, lost) = (s.udp_tx.datagrams, s.lost_packets);
        let (ds, dl) = (sent.saturating_sub(self.last_stats.0), lost.saturating_sub(self.last_stats.1));
        self.last_stats = (sent, lost);
        let loss = if ds == 0 { 0.0 } else { (dl as f32 / ds as f32 * 100.0).min(100.0) };
        if let Some(d) = &self.devices {
            d.set_loss(loss);
        }
        loss
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
