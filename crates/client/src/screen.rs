//! Transporte de vídeo (tela e câmera, Fases 4–5), sem depender de GPU:
//! cada quadro cifrado vai num stream QUIC unidirecional até o relay
//! ([`Outgoing`]), e os quadros do que estamos assistindo chegam do mesmo
//! jeito ([`spawn_incoming`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lerri_core::MemberId;
use lerri_core::media::{Codec, Header, MAX_VIDEO_FRAME, Video};
use lerri_media::screen::{Frame, Keys, open_frame, seal_frame};
use lerri_net::iroh::endpoint::Connection;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Quadro que não chega neste tempo é abandonado.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Quem recebe os quadros de um stream assistido.
pub type Sink = Arc<dyn Fn(Frame) + Send + Sync>;
/// Streams assistidos agora e para onde vão os quadros de cada um.
pub type Watching = Arc<Mutex<HashMap<(MemberId, Video), Sink>>>;

/// Saída de quadros de um stream nosso (tela ou câmera).
pub struct Outgoing {
    keys: Keys,
    video: Video,
    /// Uma sequência por camada do simulcast (o receptor reordena cada uma).
    seq: [AtomicU16; 2],
    tx: mpsc::UnboundedSender<Vec<u8>>,
    in_flight: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Outgoing {
    pub fn start(conn: Connection, keys: Keys, video: Video) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let in_flight: Arc<AtomicUsize> = Arc::default();
        let f = in_flight.clone();
        let task = tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                let (conn, f) = (conn.clone(), f.clone());
                tokio::spawn(async move {
                    let sent = tokio::time::timeout(DELIVERY_TIMEOUT, async {
                        let mut s = conn.open_uni().await?;
                        s.write_all(&data).await?;
                        s.finish()?;
                        // Em trânsito até o relay confirmar tudo.
                        let _ = s.stopped().await;
                        anyhow::Ok(())
                    })
                    .await;
                    if !matches!(sent, Ok(Ok(()))) {
                        tracing::debug!("quadro de vídeo não entregue ao relay");
                    }
                    f.fetch_sub(1, Ordering::Relaxed);
                });
            }
        });
        Arc::new(Self { keys, video, seq: [AtomicU16::new(0), AtomicU16::new(0)], tx, in_flight, task })
    }

    /// Cifra e manda um quadro codificado da camada alta. Pode ser chamado de
    /// qualquer thread (o encoder entrega na dele).
    pub fn send(&self, keyframe: bool, codec: Codec, data: &[u8]) {
        self.send_layer(lerri_core::media::LAYER_HIGH, keyframe, codec, data);
    }

    /// Como [`Outgoing::send`], na camada `layer` do simulcast.
    pub fn send_layer(&self, layer: u8, keyframe: bool, codec: Codec, data: &[u8]) {
        self.send_frame(layer, keyframe, false, codec, data);
    }

    /// Quadro da camada `layer`; `droppable`: da camada temporal de cima do
    /// SVC, que leva a sequência do último quadro de base sem avançá-la (o
    /// relay pode cortá-lo sem abrir buraco).
    pub fn send_frame(&self, layer: u8, keyframe: bool, droppable: bool, codec: Codec, data: &[u8]) {
        let counter = &self.seq[usize::from(layer.min(1))];
        let header = if droppable && !keyframe {
            Header::video_droppable(self.video, layer, counter.load(Ordering::Relaxed).wrapping_sub(1))
        } else {
            Header::video(self.video, layer, counter.fetch_add(1, Ordering::Relaxed), keyframe)
        };
        match seal_frame(&self.keys, header, codec, data) {
            Ok(wire) => {
                self.in_flight.fetch_add(1, Ordering::Relaxed);
                if self.tx.send(wire).is_err() {
                    self.in_flight.fetch_sub(1, Ordering::Relaxed);
                }
            }
            // Sem chave (fora do grupo): não manda nada em claro.
            Err(e) => tracing::debug!("quadro de vídeo não enviado: {e}"),
        }
    }

    /// Quadros enviados ainda sem confirmação do relay.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Lê os quadros que o relay manda (só os do que estamos assistindo) e
/// entrega cada um ao seu destino.
pub fn spawn_incoming(conn: Connection, keys: Keys, watching: Watching) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(mut recv) = conn.accept_uni().await {
            let (keys, watching) = (keys.clone(), watching.clone());
            tokio::spawn(async move {
                let Ok(data) = recv.read_to_end(MAX_VIDEO_FRAME).await else { return };
                let frame = match open_frame(&keys, &data) {
                    Ok(f) => f,
                    Err(e) => return tracing::debug!("quadro de vídeo descartado: {e}"),
                };
                let sink = watching.lock().unwrap().get(&(frame.from, frame.video)).cloned();
                if let Some(sink) = sink {
                    sink(frame);
                }
            });
        }
    })
}
