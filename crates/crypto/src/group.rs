//! Grupo MLS de uma sala (RFC 9420, SPEC-02 §4.1).
//!
//! A credencial de cada membro é a chave pública da identidade dele, e quem
//! assina no MLS é a própria chave de identidade. Assim o `MemberId`
//! (SHA-256 da chave) sai direto da árvore do grupo, sem confiar no relay.
//!
//! Commits vão como `PublicMessage` (o relay vê quem entra e sai, o que ele já
//! sabe); mensagens de aplicação vão sempre cifradas.

use std::collections::HashMap;
use std::sync::RwLock;

use lerri_core::{MemberId, RoomId};
use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_memory_storage::MemoryStorage;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;
use sha2::{Digest, Sha256};

use crate::Identity;

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
/// Epochs anteriores cujas chaves ficam guardadas, para decifrar mensagens que
/// chegam logo depois de um commit.
const MAX_PAST_EPOCHS: usize = 5;
/// Mensagens de aplicação são completadas até um múltiplo disto (esconde o
/// tamanho exato).
const PADDING: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("sem grupo MLS ainda (aguardando admissão)")]
    NoGroup,
    #[error("mensagem MLS malformada")]
    Malformed,
    #[error("KeyPackage inválido: {0}")]
    BadKeyPackage(String),
    #[error("a credencial não corresponde à chave de assinatura")]
    CredentialMismatch,
    #[error("{0} não está no grupo")]
    NotInGroup(String),
    #[error("MLS: {0}")]
    Mls(String),
}

fn mls<E: std::fmt::Debug>(e: E) -> GroupError {
    GroupError::Mls(format!("{e:?}"))
}

/// Provedor do openmls: criptografia RustCrypto + armazenamento em memória que
/// a gente serializa para o disco.
#[derive(Default)]
struct Provider {
    crypto: RustCrypto,
    storage: MemoryStorage,
}

impl OpenMlsProvider for Provider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }
    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }
    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}

/// Commit pronto para mandar ao relay.
#[derive(Debug)]
pub struct OutgoingCommit {
    /// Epoch em que o commit foi criado (o relay ordena por ele).
    pub epoch: u64,
    pub data: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    pub adds: Vec<MemberId>,
    pub removes: Vec<MemberId>,
}

/// Resultado de processar uma mensagem recebida.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    App { sender: MemberId, data: Vec<u8> },
    /// Commit de outro membro já aplicado.
    Commit { committer: MemberId, added: Vec<MemberId>, removed: Vec<MemberId>, self_removed: bool },
    /// Era o nosso commit pendente voltando do relay (já aplicado).
    OwnCommit,
    Ignored,
}

pub struct GroupState {
    provider: Provider,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
    group: Option<MlsGroup>,
    group_id: GroupId,
}

/// `MemberId` de uma credencial MLS: a identidade é a chave Ed25519 crua.
pub fn member_of(credential: &Credential) -> Option<MemberId> {
    let basic = BasicCredential::try_from(credential.clone()).ok()?;
    let key: [u8; 32] = basic.identity().try_into().ok()?;
    Some(MemberId(Sha256::digest(key).into()))
}

impl GroupState {
    /// Estado vazio de uma sala: ainda não está no grupo.
    pub fn new(identity: &Identity, room: RoomId) -> Self {
        let public = identity.verifying_key().to_bytes().to_vec();
        let signer = SignatureKeyPair::from_raw(SignatureScheme::ED25519, identity.secret_bytes().to_vec(), public.clone());
        let credential = CredentialWithKey {
            credential: BasicCredential::new(public.clone()).into(),
            signature_key: public.into(),
        };
        Self { provider: Provider::default(), signer, credential, group: None, group_id: GroupId::from_slice(&room.0) }
    }

    /// Restaura de [`GroupState::snapshot`].
    pub fn restore(identity: &Identity, room: RoomId, snapshot: &[u8]) -> Result<Self, GroupError> {
        let mut st = Self::new(identity, room);
        let values: HashMap<Vec<u8>, Vec<u8>> = postcard::from_bytes(snapshot).map_err(|_| GroupError::Malformed)?;
        st.provider.storage = MemoryStorage { values: RwLock::new(values) };
        st.group = MlsGroup::load(st.provider.storage(), &st.group_id).map_err(mls)?;
        Ok(st)
    }

    /// Estado completo (chaves do grupo inclusive): só vai para o disco cifrado.
    pub fn snapshot(&self) -> Vec<u8> {
        let values = self.provider.storage.values.read().unwrap();
        postcard::to_allocvec(&*values).expect("serializar mapa não falha")
    }

    pub fn in_group(&self) -> bool {
        self.group.as_ref().is_some_and(MlsGroup::is_active)
    }

    pub fn epoch(&self) -> u64 {
        self.group.as_ref().map_or(0, |g| g.epoch().as_u64())
    }

    pub fn has_pending_commit(&self) -> bool {
        self.group.as_ref().is_some_and(|g| g.pending_commit().is_some())
    }

    pub fn members(&self) -> Vec<MemberId> {
        self.group.as_ref().map_or_else(Vec::new, |g| g.members().filter_map(|m| member_of(&m.credential)).collect())
    }

    fn create_config() -> MlsGroupCreateConfig {
        MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .padding_size(PADDING)
            .wire_format_policy(MIXED_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build()
    }

    fn join_config() -> MlsGroupJoinConfig {
        MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .padding_size(PADDING)
            .wire_format_policy(MIXED_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build()
    }

    /// Cria o grupo (quem cria a sala).
    pub fn create(&mut self) -> Result<(), GroupError> {
        let group = MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &Self::create_config(),
            self.group_id.clone(),
            self.credential.clone(),
        )
        .map_err(mls)?;
        self.group = Some(group);
        Ok(())
    }

    /// KeyPackage novo, serializado, para pedir entrada.
    pub fn key_package(&mut self) -> Result<Vec<u8>, GroupError> {
        let bundle = KeyPackage::builder()
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential.clone())
            .map_err(mls)?;
        bundle.key_package().tls_serialize_detached().map_err(mls)
    }

    /// Valida um KeyPackage recebido e devolve o `MemberId` que ele carrega.
    /// Exige que a credencial seja a mesma chave que assina (SPEC-02 §1).
    pub fn check_key_package(&self, data: &[u8]) -> Result<(KeyPackage, MemberId), GroupError> {
        let kp_in = KeyPackageIn::tls_deserialize_exact(data).map_err(|_| GroupError::Malformed)?;
        let kp = kp_in
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| GroupError::BadKeyPackage(format!("{e:?}")))?;
        if kp.ciphersuite() != CIPHERSUITE {
            return Err(GroupError::BadKeyPackage("suíte diferente".into()));
        }
        let leaf = kp.leaf_node();
        let basic = BasicCredential::try_from(leaf.credential().clone()).map_err(|_| GroupError::CredentialMismatch)?;
        if basic.identity() != leaf.signature_key().as_slice() {
            return Err(GroupError::CredentialMismatch);
        }
        let member = member_of(leaf.credential()).ok_or(GroupError::CredentialMismatch)?;
        Ok((kp, member))
    }

    /// Entra no grupo por um Welcome. Devolve quem o emitiu.
    pub fn join(&mut self, welcome: &[u8]) -> Result<MemberId, GroupError> {
        let msg = MlsMessageIn::tls_deserialize_exact(welcome).map_err(|_| GroupError::Malformed)?;
        let MlsMessageBodyIn::Welcome(welcome) = msg.extract() else { return Err(GroupError::Malformed) };
        let staged =
            StagedWelcome::new_from_welcome(&self.provider, &Self::join_config(), welcome, None).map_err(mls)?;
        let sender = staged.welcome_sender().map_err(mls)?;
        let sender = member_of(sender.credential()).ok_or(GroupError::CredentialMismatch)?;
        let group = staged.into_group(&self.provider).map_err(mls)?;
        if group.group_id() != &self.group_id {
            return Err(GroupError::Mls("Welcome de outro grupo".into()));
        }
        self.group = Some(group);
        Ok(sender)
    }

    fn leaf_of(&self, member: MemberId) -> Option<LeafNodeIndex> {
        self.group.as_ref()?.members().find(|m| member_of(&m.credential) == Some(member)).map(|m| m.index)
    }

    /// Adiciona (e, se já estiver no grupo, troca o leaf antigo: readição).
    pub fn add(&mut self, key_package: &[u8]) -> Result<OutgoingCommit, GroupError> {
        let (kp, member) = self.check_key_package(key_package)?;
        let old = self.leaf_of(member);
        let epoch = self.epoch();
        let provider = &self.provider;
        let signer = &self.signer;
        let group = self.group.as_mut().ok_or(GroupError::NoGroup)?;
        let (commit, welcome) = match old {
            Some(leaf) => {
                let m = group.swap_members(provider, signer, &[leaf], &[kp]).map_err(mls)?;
                (m.commit, m.welcome)
            }
            None => {
                let (commit, welcome, _) = group.add_members(provider, signer, &[kp]).map_err(mls)?;
                (commit, welcome)
            }
        };
        Ok(OutgoingCommit {
            epoch,
            data: commit.tls_serialize_detached().map_err(mls)?,
            welcome: Some(welcome.tls_serialize_detached().map_err(mls)?),
            adds: vec![member],
            removes: Vec::new(),
        })
    }

    pub fn remove(&mut self, member: MemberId) -> Result<OutgoingCommit, GroupError> {
        let leaf = self.leaf_of(member).ok_or_else(|| GroupError::NotInGroup(member.short()))?;
        let epoch = self.epoch();
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().ok_or(GroupError::NoGroup)?;
        let (commit, _, _) = group.remove_members(provider, signer, &[leaf]).map_err(mls)?;
        Ok(OutgoingCommit {
            epoch,
            data: commit.tls_serialize_detached().map_err(mls)?,
            welcome: None,
            adds: Vec::new(),
            removes: vec![member],
        })
    }

    /// Commit que renova a própria chave (segurança pós-comprometimento).
    pub fn self_update(&mut self) -> Result<OutgoingCommit, GroupError> {
        let epoch = self.epoch();
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().ok_or(GroupError::NoGroup)?;
        let bundle = group.self_update(provider, signer, LeafNodeParameters::default()).map_err(mls)?;
        Ok(OutgoingCommit {
            epoch,
            data: bundle.commit().tls_serialize_detached().map_err(mls)?,
            welcome: None,
            adds: Vec::new(),
            removes: Vec::new(),
        })
    }

    /// O relay aceitou nosso commit.
    pub fn confirm_pending(&mut self) -> Result<(), GroupError> {
        let provider = &self.provider;
        self.group.as_mut().ok_or(GroupError::NoGroup)?.merge_pending_commit(provider).map_err(mls)
    }

    /// Outro commit ganhou o epoch: descarta o nosso.
    pub fn discard_pending(&mut self) -> Result<(), GroupError> {
        let storage = self.provider.storage();
        self.group.as_mut().ok_or(GroupError::NoGroup)?.clear_pending_commit(storage).map_err(mls)
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, GroupError> {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().ok_or(GroupError::NoGroup)?;
        let msg = group.create_message(provider, signer, plaintext).map_err(mls)?;
        msg.tls_serialize_detached().map_err(mls)
    }

    /// Processa commit ou mensagem de aplicação vinda do relay.
    pub fn process(&mut self, data: &[u8]) -> Result<Incoming, GroupError> {
        let msg = MlsMessageIn::tls_deserialize_exact(data).map_err(|_| GroupError::Malformed)?;
        let proto = msg.try_into_protocol_message().map_err(|_| GroupError::Malformed)?;
        let provider = &self.provider;
        let group = self.group.as_mut().ok_or(GroupError::NoGroup)?;
        if proto.epoch() < group.epoch() && proto.content_type() == ContentType::Commit {
            return Ok(Incoming::Ignored);
        }
        let processed = group.process_message(provider, proto).map_err(mls)?;
        let sender = member_of(processed.credential()).ok_or(GroupError::CredentialMismatch)?;
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Incoming::App { sender, data: app.into_bytes() }),
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let added = staged
                    .add_proposals()
                    .filter_map(|p| member_of(p.add_proposal().key_package().leaf_node().credential()))
                    .collect();
                let removed_leaves: Vec<LeafNodeIndex> =
                    staged.remove_proposals().map(|p| p.remove_proposal().removed()).collect();
                let removed = group
                    .members()
                    .filter(|m| removed_leaves.contains(&m.index))
                    .filter_map(|m| member_of(&m.credential))
                    .collect();
                let self_removed = staged.self_removed();
                group.merge_staged_commit(provider, *staged).map_err(mls)?;
                Ok(Incoming::Commit { committer: sender, added, removed, self_removed })
            }
            ProcessedMessageContent::OwnPendingCommit => {
                group.merge_pending_commit(provider).map_err(mls)?;
                Ok(Incoming::OwnCommit)
            }
            _ => Ok(Incoming::Ignored),
        }
    }

    /// Segredo exportado do epoch atual (SFrame usa na Fase 3).
    pub fn export_secret(&self, label: &str, len: usize) -> Result<Vec<u8>, GroupError> {
        let g = self.group.as_ref().ok_or(GroupError::NoGroup)?;
        g.export_secret(self.provider.crypto(), label, &[], len).map_err(mls)
    }
}

/// Prova de admissão `HMAC(HKDF(s), KeyPackage)` (SPEC-02 §3.3).
pub fn admission_mac(secret: &[u8; 32], key_package: &[u8]) -> [u8; 32] {
    use hmac::{KeyInit, Mac};
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(None, secret)
        .expand(b"lerrichat/admission/v1", key.as_mut())
        .expect("32 bytes cabem no HKDF-SHA256");
    let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(key.as_ref()).expect("HMAC aceita qualquer chave");
    mac.update(key_package);
    mac.finalize().into_bytes().into()
}

pub fn check_admission_mac(secret: &[u8; 32], key_package: &[u8], mac: &[u8; 32]) -> bool {
    crate::ct_eq(&admission_mac(secret, key_package), mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> RoomId {
        RoomId([7; 16])
    }

    struct Peer {
        id: Identity,
        g: GroupState,
    }

    fn peer() -> Peer {
        let id = Identity::generate().unwrap();
        let g = GroupState::new(&id, room());
        Peer { id, g }
    }

    #[test]
    fn create_add_chat_remove() {
        let (mut a, mut b, mut c) = (peer(), peer(), peer());
        a.g.create().unwrap();
        // b entra.
        let kp = b.g.key_package().unwrap();
        let commit = a.g.add(&kp).unwrap();
        assert_eq!(commit.adds, vec![b.id.member_id()]);
        a.g.confirm_pending().unwrap();
        assert_eq!(b.g.join(commit.welcome.as_ref().unwrap()).unwrap(), a.id.member_id());
        assert_eq!(a.g.epoch(), b.g.epoch());

        // Chat.
        let ct = a.g.encrypt(b"oi b").unwrap();
        assert!(!ct.windows(4).any(|w| w == b"oi b"), "texto em claro no fio");
        assert_eq!(b.g.process(&ct).unwrap(), Incoming::App { sender: a.id.member_id(), data: b"oi b".to_vec() });

        // c entra por b; a processa o commit de b.
        let commit = b.g.add(&c.g.key_package().unwrap()).unwrap();
        b.g.confirm_pending().unwrap();
        let r = a.g.process(&commit.data).unwrap();
        assert!(matches!(r, Incoming::Commit { added, .. } if added == vec![c.id.member_id()]));
        c.g.join(commit.welcome.as_ref().unwrap()).unwrap();
        assert_eq!(a.g.members().len(), 3);

        // a remove b: b não lê o que vem depois.
        let commit = a.g.remove(b.id.member_id()).unwrap();
        a.g.confirm_pending().unwrap();
        let r = b.g.process(&commit.data).unwrap();
        assert!(matches!(r, Incoming::Commit { self_removed: true, .. }));
        assert!(!b.g.in_group());
        c.g.process(&commit.data).unwrap();
        let ct = a.g.encrypt(b"segredo").unwrap();
        assert!(b.g.process(&ct).is_err());
        assert!(matches!(c.g.process(&ct).unwrap(), Incoming::App { .. }));
    }

    #[test]
    fn concurrent_commits_one_wins() {
        let (mut a, mut b) = (peer(), peer());
        a.g.create().unwrap();
        let commit = a.g.add(&b.g.key_package().unwrap()).unwrap();
        a.g.confirm_pending().unwrap();
        b.g.join(commit.welcome.as_ref().unwrap()).unwrap();

        // Os dois fazem commit no mesmo epoch; o relay aceita o de a.
        let ca = a.g.self_update().unwrap();
        let _cb = b.g.self_update().unwrap();
        a.g.confirm_pending().unwrap();
        b.g.discard_pending().unwrap();
        b.g.process(&ca.data).unwrap();
        assert_eq!(a.g.epoch(), b.g.epoch());
        assert_eq!(a.g.export_secret("t", 32).unwrap(), b.g.export_secret("t", 32).unwrap());
    }

    #[test]
    fn snapshot_roundtrip_keeps_keys() {
        let (mut a, mut b) = (peer(), peer());
        a.g.create().unwrap();
        let commit = a.g.add(&b.g.key_package().unwrap()).unwrap();
        a.g.confirm_pending().unwrap();
        b.g.join(commit.welcome.as_ref().unwrap()).unwrap();
        let snap = b.g.snapshot();
        let mut b2 = GroupState::restore(&b.id, room(), &snap).unwrap();
        let ct = a.g.encrypt(b"depois de reiniciar").unwrap();
        assert!(matches!(b2.process(&ct).unwrap(), Incoming::App { .. }));
    }

    #[test]
    fn rejects_key_package_with_foreign_credential() {
        let a = peer();
        let mut fake = GroupState::new(&a.id, room());
        // Credencial diz ser outra pessoa, chave de assinatura é de `a`.
        fake.credential.credential = BasicCredential::new(vec![9; 32]).into();
        let kp = fake.key_package().unwrap();
        assert!(matches!(a.g.check_key_package(&kp), Err(GroupError::CredentialMismatch)));
    }

    #[test]
    fn admission_mac_binds_secret_and_key_package() {
        let mac = admission_mac(&[1; 32], b"kp");
        assert!(check_admission_mac(&[1; 32], b"kp", &mac));
        assert!(!check_admission_mac(&[2; 32], b"kp", &mac));
        assert!(!check_admission_mac(&[1; 32], b"kp2", &mac));
    }
}
