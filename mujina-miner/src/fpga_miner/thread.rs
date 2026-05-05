//! Scheduler-facing Tang Nano hash thread.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use async_trait::async_trait;
use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::Encodable;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc as tokio_mpsc;

use super::TangNano9kConfig;
use crate::{
    asic::hash_thread::{
        HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
        HashThreadStatus, Share,
    },
    job_source::MerkleRootKind,
    tracing::prelude::*,
    transport::SerialStream,
    types::HashRate,
};

const TNJ_RESPONSE_LEN: usize = 37;
const TNJ_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Commands sent to the FPGA worker thread.
#[derive(Debug)]
enum FpgaCommand {
    UpdateTask {
        task: HashTask,
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    ReplaceTask {
        task: HashTask,
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    GoIdle {
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    Shutdown,
}

/// One Tang Nano FPGA bitstream instance as a Mujina `HashThread`.
pub struct TangNano9kHashThread {
    name: String,
    command_tx: mpsc::Sender<FpgaCommand>,
    #[expect(dead_code)]
    event_tx: tokio_mpsc::Sender<HashThreadEvent>,
    event_rx: Option<tokio_mpsc::Receiver<HashThreadEvent>>,
    status: Arc<RwLock<HashThreadStatus>>,
    capabilities: HashThreadCapabilities,
    shutdown: Arc<AtomicBool>,
    _thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl TangNano9kHashThread {
    pub fn new(name: String, config: TangNano9kConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = tokio_mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let status_clone = Arc::clone(&status);
        let shutdown_clone = Arc::clone(&shutdown);
        let thread_name = name.clone();

        let handle = std::thread::Builder::new()
            .name(format!("tang-nano-fpga-{}", name))
            .spawn(move || run_fpga_loop(thread_name, config, cmd_rx, status_clone, shutdown_clone))
            .expect("failed to spawn Tang Nano FPGA thread");

        Self {
            name,
            command_tx: cmd_tx,
            event_tx: evt_tx,
            event_rx: Some(evt_rx),
            status,
            capabilities: HashThreadCapabilities {
                // One iterative core at 27 MHz, two compression passes per nonce.
                hashrate_estimate: HashRate(200_000),
            },
            shutdown,
            _thread_handle: Some(handle),
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.command_tx.send(FpgaCommand::Shutdown);
    }
}

impl Drop for TangNano9kHashThread {
    fn drop(&mut self) {
        TangNano9kHashThread::request_shutdown(self);
    }
}

#[async_trait]
impl HashThread for TangNano9kHashThread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(FpgaCommand::UpdateTask {
                task: new_task,
                response_tx,
            })
            .map_err(|_| HashThreadError::ChannelClosed("fpga command channel closed".into()))?;

        response_rx.await.map_err(|_| {
            HashThreadError::WorkAssignmentFailed("no response from FPGA thread".into())
        })?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(FpgaCommand::ReplaceTask {
                task: new_task,
                response_tx,
            })
            .map_err(|_| HashThreadError::ChannelClosed("fpga command channel closed".into()))?;

        response_rx.await.map_err(|_| {
            HashThreadError::WorkAssignmentFailed("no response from FPGA thread".into())
        })?
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(FpgaCommand::GoIdle { response_tx })
            .map_err(|_| HashThreadError::ChannelClosed("fpga command channel closed".into()))?;

        response_rx.await.map_err(|_| {
            HashThreadError::WorkAssignmentFailed("no response from FPGA thread".into())
        })?
    }

    async fn shutdown(&mut self) -> Result<(), HashThreadError> {
        self.request_shutdown();
        Ok(())
    }

    fn take_event_receiver(&mut self) -> Option<tokio_mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

fn run_fpga_loop(
    thread_name: String,
    config: TangNano9kConfig,
    cmd_rx: mpsc::Receiver<FpgaCommand>,
    status: Arc<RwLock<HashThreadStatus>>,
    shutdown: Arc<AtomicBool>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            error!(%error, "failed to create FPGA worker runtime");
            return;
        }
    };

    let mut current_task: Option<HashTask> = None;
    let mut shares_found = 0u64;

    while !shutdown.load(Ordering::Relaxed) {
        let command = match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        match command {
            FpgaCommand::UpdateTask { task, response_tx }
            | FpgaCommand::ReplaceTask { task, response_tx } => {
                let old = current_task.replace(task.clone());
                update_status(&status, true, shares_found, HashRate(0));
                let _ = response_tx.send(Ok(old));

                match rt.block_on(run_one_fpga_job(&thread_name, &config, task)) {
                    Ok(Some(share)) => {
                        shares_found += 1;
                        update_status(&status, true, shares_found, HashRate(200_000));
                        if let Some(task) = &current_task {
                            let _ = task.share_tx.blocking_send(share);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(%error, "Tang Nano FPGA job failed");
                        let mut s = status.write().unwrap();
                        s.hardware_errors += 1;
                    }
                }
            }
            FpgaCommand::GoIdle { response_tx } => {
                let old = current_task.take();
                update_status(&status, false, shares_found, HashRate(0));
                let _ = response_tx.send(Ok(old));
            }
            FpgaCommand::Shutdown => return,
        }
    }
}

async fn run_one_fpga_job(
    thread_name: &str,
    config: &TangNano9kConfig,
    task: HashTask,
) -> anyhow::Result<Option<Share>> {
    let merkle_root = match &task.template.merkle_root {
        MerkleRootKind::Fixed(root) => *root,
        MerkleRootKind::Computed(_) => {
            let en2 = task
                .en2
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("computed-merkle task missing extranonce2"))?;
            task.template.compute_merkle_root(en2)?
        }
    };

    let header = BlockHeader {
        version: task.template.version.base(),
        prev_blockhash: task.template.prev_blockhash,
        merkle_root,
        time: task.ntime,
        bits: task.template.bits,
        nonce: 0,
    };

    let mut header_bytes = Vec::with_capacity(80);
    header.consensus_encode(&mut header_bytes)?;
    anyhow::ensure!(
        header_bytes.len() == 80,
        "block header serialized to non-80-byte length"
    );

    let packet = make_tnj_packet(&header_bytes, task.share_target.to_be_bytes());

    debug!(
        thread = %thread_name,
        port = %config.port,
        header_hex = %hex::encode(&header_bytes),
        "sending Tang Nano FPGA work"
    );

    let response = tokio::time::timeout(TNJ_RESPONSE_TIMEOUT, async {
        let serial = SerialStream::new(&config.port, config.baud)?;
        let (mut reader, mut writer, _control) = serial.split();
        writer.write_all(&packet).await?;
        writer.flush().await?;

        let mut response = [0u8; TNJ_RESPONSE_LEN];
        reader.read_exact(&mut response).await?;
        anyhow::Ok(response)
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for Tang Nano FPGA response after {:?}",
            TNJ_RESPONSE_TIMEOUT
        )
    })??;
    anyhow::ensure!(
        response[0] == b'F',
        "unexpected FPGA response tag 0x{:02x}",
        response[0]
    );

    let fpga_nonce_bytes: [u8; 4] = response[1..5].try_into()?;
    // The bitstream appends current_nonce as a SHA big-endian word. Bitcoin
    // consensus serializes Header::nonce little-endian, so use the returned
    // bytes as wire-order and convert them into Rust's u32 field value.
    let nonce = u32::from_le_bytes(fpga_nonce_bytes);
    let result_header = BlockHeader { nonce, ..header };
    let hash = result_header.block_hash();

    if task.share_target.is_met_by(hash) {
        Ok(Some(Share {
            nonce,
            hash,
            version: task.template.version.base(),
            ntime: task.ntime,
            extranonce2: task.en2,
            expected_work: task.share_target.to_work(),
        }))
    } else {
        warn!(
            nonce = format!("{:#010x}", nonce),
            hash = %hash,
            fpga_hash_hex = %hex::encode(&response[5..]),
            "FPGA nonce did not meet Mujina target after host validation"
        );
        Ok(None)
    }
}

fn make_tnj_packet(header_bytes: &[u8], target_be: [u8; 32]) -> Vec<u8> {
    let midstate = sha256_midstate(&header_bytes[..64]);
    let mut packet = Vec::with_capacity(79);
    packet.extend_from_slice(b"TNJ");
    packet.extend_from_slice(&midstate);
    packet.extend_from_slice(&header_bytes[64..76]);
    packet.extend_from_slice(&target_be);
    packet
}

fn update_status(
    status: &Arc<RwLock<HashThreadStatus>>,
    is_active: bool,
    shares_found: u64,
    hashrate: HashRate,
) {
    let mut s = status.write().unwrap();
    s.is_active = is_active;
    s.chip_shares_found = shares_found;
    s.hashrate = hashrate;
}

fn sha256_midstate(block: &[u8]) -> [u8; 32] {
    debug_assert_eq!(block.len(), 64);
    let state = sha256_compress(SHA256_IV, block.try_into().unwrap());
    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_compress(mut state: [u32; 8], block: [u8; 64]) -> [u32; 8] {
    let mut w = [0u32; 64];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes(chunk.try_into().unwrap());
    }
    for i in 16..64 {
        w[i] = small_sigma1(w[i - 2])
            .wrapping_add(w[i - 7])
            .wrapping_add(small_sigma0(w[i - 15]))
            .wrapping_add(w[i - 16]);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
    for i in 0..64 {
        let t1 = h
            .wrapping_add(big_sigma1(e))
            .wrapping_add((e & f) ^ (!e & g))
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let t2 = big_sigma0(a).wrapping_add((a & b) ^ (a & c) ^ (b & c));
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
    state
}

fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}

fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}

fn small_sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

fn small_sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::block::Version;
    use bitcoin::hashes::sha256d;
    use bitcoin::hashes::{Hash, HashEngine};
    use bitcoin::pow::{CompactTarget, Target};

    #[test]
    fn tnj_packet_matches_known_genesis_payload() {
        let header = BlockHeader {
            version: Version::from_consensus(1),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            merkle_root: bitcoin::TxMerkleNode::from_byte_array([
                0x3b, 0xa3, 0xed, 0xfd, 0x7a, 0x7b, 0x12, 0xb2, 0x7a, 0xc7, 0x2c, 0x3e, 0x67, 0x76,
                0x8f, 0x61, 0x7f, 0xc8, 0x1b, 0xc3, 0x88, 0x8a, 0x51, 0x32, 0x3a, 0x9f, 0xb8, 0xaa,
                0x4b, 0x1e, 0x5e, 0x4a,
            ]),
            time: 0x495fab29,
            bits: CompactTarget::from_consensus(0x1d00ffff),
            nonce: 0x7c2bac1d,
        };

        let mut header_bytes = Vec::new();
        header.consensus_encode(&mut header_bytes).unwrap();
        let packet = make_tnj_packet(&header_bytes, Target::MAX.to_be_bytes());

        assert_eq!(packet.len(), 79);
        assert_eq!(&packet[..3], b"TNJ");
        assert_eq!(
            hex::encode(&packet[3..35]),
            "bc909a336358bff090ccac7d1e59caa8c3c8d8e94f0103c896b187364719f91b"
        );
        assert_eq!(hex::encode(&packet[35..47]), "4b1e5e4a29ab5f49ffff001d");
    }

    #[test]
    fn target_bytes_for_fpga_are_pow_integer_big_endian() {
        let target = Target::from(CompactTarget::from_consensus(0x1d00ffff));
        assert_eq!(
            hex::encode(target.to_be_bytes()),
            "00000000ffff0000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex::encode(target.to_le_bytes()),
            "0000000000000000000000000000000000000000000000000000ffff00000000"
        );
    }

    #[test]
    fn fpga_log_response_matches_reconstructed_header_hash() {
        let mut header = hex::decode(
            "0000002009f9caa3f91506e721ef19894ccecf472a0d5f1064ad01000000000000000000d33f1e89c29931c8f6daf3eafe9c28e798666c772acf8fd723fb2c42ba8a518bc30dfa69f01f021700000000",
        )
        .unwrap();
        let fpga_nonce_bytes = [0x00, 0x04, 0x26, 0x88];
        header[76..80].copy_from_slice(&fpga_nonce_bytes);

        let mut engine = sha256d::Hash::engine();
        engine.input(&header);
        let digest = sha256d::Hash::from_engine(engine);

        assert_eq!(
            hex::encode(digest.as_byte_array()),
            "0000508d9c92ab5788bf90f7356914f1b73030f0e0523d6b50474ae2eb486d52"
        );

        let parsed_nonce = u32::from_le_bytes(fpga_nonce_bytes);
        assert_eq!(parsed_nonce, 0x88260400);
    }
}
